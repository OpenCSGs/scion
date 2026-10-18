//! EventService：按 seq 增量读取 ref 变化（拉取方式，docs/guide/events.md）。

use std::sync::Arc;

use bytes::Bytes;
use scion_auth::{Action, Guard};
use scion_proto::v1::event_service_server::EventService;
use scion_proto::v1::{ListRefEventsRequest, ListRefEventsResponse, RefChangeEvent};
use scion_store::MetaStore;
use tonic::{Request, Response, Status};

use crate::auth::{authorize_any, principal, to_status};
use crate::status;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

pub struct EventSvc {
    meta: Arc<dyn MetaStore>,
    guard: Arc<Guard>,
}

impl EventSvc {
    pub fn new(meta: Arc<dyn MetaStore>) -> Self {
        Self {
            meta,
            guard: Guard::disabled(),
        }
    }

    pub fn with_guard(mut self, guard: Arc<Guard>) -> Self {
        self.guard = guard;
        self
    }
}

fn id_bytes(id: Option<scion_object::ObjectId>) -> Bytes {
    id.map(|i| Bytes::copy_from_slice(&i.to_bytes())).unwrap_or_default()
}

#[tonic::async_trait]
impl EventService for EventSvc {
    async fn list_ref_events(
        &self,
        request: Request<ListRefEventsRequest>,
    ) -> Result<Response<ListRefEventsResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        if req.repo.is_empty() {
            // 全部仓库的变化：系统管理员
            if !self.guard.is_admin(&p).await.map_err(to_status)? {
                return Err(if p.authenticated {
                    to_status(scion_auth::AuthError::Denied {
                        repo: "*".into(),
                        action: "admin",
                        reference: None,
                        path: None,
                    })
                } else {
                    Status::unauthenticated("authentication required")
                });
            }
        } else {
            authorize_any(&self.guard, &p, &req.repo, Action::Read).await?;
        }
        let limit = match req.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        } as usize;
        let repo = Some(req.repo.as_str()).filter(|r| !r.is_empty());
        let events = self
            .meta
            .list_ref_events(req.after_seq, limit, repo)
            .await
            .map_err(status::from_store)?;
        let (oldest, last) = self.meta.ref_event_bounds().await.map_err(status::from_store)?;
        // after_seq 之后有过事件，但最早的那些已被清理
        let truncated = last > req.after_seq && oldest.is_none_or(|lo| lo > req.after_seq.saturating_add(1));
        Ok(Response::new(ListRefEventsResponse {
            next_seq: events.last().map_or(req.after_seq, |e| e.seq),
            events: events
                .into_iter()
                .map(|e| RefChangeEvent {
                    seq: e.seq,
                    repo: e.repo,
                    r#ref: e.name,
                    old_changeset_id: id_bytes(e.old_target),
                    new_changeset_id: id_bytes(e.new_target),
                    version: e.version,
                    created_at_ms: e.created_at_ms,
                })
                .collect(),
            truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_store::{MemStore, RefExpect};
    use tonic::Code;

    fn req(after_seq: u64, limit: u32, repo: &str) -> Request<ListRefEventsRequest> {
        Request::new(ListRefEventsRequest {
            after_seq,
            limit,
            repo: repo.into(),
        })
    }

    #[tokio::test]
    async fn lists_pages_and_reports_truncation() {
        let store = Arc::new(MemStore::new());
        store.create_repo("a").await.expect("repo");
        store.create_repo("b").await.expect("repo");
        let x = scion_object::ObjectId::hash(scion_object::ObjectKind::Changeset, b"x");
        store.cas_ref("a", "main", RefExpect::Absent, x).await.expect("a");
        store.cas_ref("b", "main", RefExpect::Absent, x).await.expect("b");
        store.delete_ref("a", "main", None).await.expect("delete");
        let svc = EventSvc::new(store.clone());
        let page = svc.list_ref_events(req(0, 2, "")).await.expect("list").into_inner();
        assert_eq!(page.events.len(), 2);
        assert_eq!((page.next_seq, page.truncated), (2, false));
        assert_eq!(page.events[0].repo, "a");
        assert!(page.events[0].old_changeset_id.is_empty());
        let rest = svc.list_ref_events(req(2, 0, "")).await.expect("rest").into_inner();
        assert_eq!(rest.events.len(), 1);
        assert!(rest.events[0].new_changeset_id.is_empty(), "deleted");
        let idle = svc.list_ref_events(req(3, 0, "")).await.expect("idle").into_inner();
        assert_eq!((idle.events.len(), idle.next_seq, idle.truncated), (0, 3, false));
        let only_b = svc.list_ref_events(req(0, 0, "b")).await.expect("b").into_inner();
        assert_eq!(only_b.events.len(), 1);
        // 清理后落后的消费方得到 truncated
        store.prune_ref_events(i64::MAX).await.expect("prune");
        let behind = svc.list_ref_events(req(1, 0, "")).await.expect("behind").into_inner();
        assert!(behind.truncated && behind.events.is_empty());
        let current = svc.list_ref_events(req(3, 0, "")).await.expect("current").into_inner();
        assert!(!current.truncated);
    }

    #[tokio::test]
    async fn all_repos_need_system_admin() {
        use scion_auth::{GuardConfig, Principal};
        let store = Arc::new(MemStore::new());
        store.create_repo("a").await.expect("repo");
        let guard = Guard::build(
            GuardConfig {
                required: true,
                ..GuardConfig::default()
            },
            store.clone(),
            Arc::new(|_: &str| None),
        )
        .expect("guard");
        guard
            .set_policy(
                r#"
                [[rule]]
                subjects = ["user:alice"]
                repos = ["a"]
                actions = ["read"]

                [[rule]]
                subjects = ["user:root"]
                repos = ["*"]
                actions = ["admin"]
                "#,
            )
            .await
            .expect("policy");
        let svc = EventSvc::new(store).with_guard(guard);
        let as_user = |r: Request<ListRefEventsRequest>, who: &str| {
            let mut r = r;
            r.extensions_mut().insert(Principal::user(who, vec![]));
            r
        };
        assert!(svc.list_ref_events(as_user(req(0, 0, "a"), "alice")).await.is_ok());
        let err = svc
            .list_ref_events(as_user(req(0, 0, ""), "alice"))
            .await
            .expect_err("all repos");
        assert_eq!(err.code(), Code::PermissionDenied);
        let err = svc.list_ref_events(req(0, 0, "")).await.expect_err("anonymous");
        assert_eq!(err.code(), Code::Unauthenticated);
        assert!(svc.list_ref_events(as_user(req(0, 0, ""), "root")).await.is_ok());
    }
}
