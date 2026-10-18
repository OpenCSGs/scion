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
/// 按仓库拉取时，早于仓库创建时间这么久的事件属于此前同名的旧仓库（容忍节点间的时钟偏差）。
/// 名字至少在删除 2 小时后才能重新使用，旧仓库的事件不会落在这个窗口里。
const CLOCK_SKEW_MS: i64 = 10 * 60 * 1000;

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
        // 事件按仓库名保存，清除后的旧仓库与重新创建的同名仓库共用名字：只返回当前仓库创建之后的事件
        let since = match repo {
            Some(r) => match self.meta.get_repo(r).await.map_err(status::from_store)? {
                Some(info) => info.created_at_ms.saturating_sub(CLOCK_SKEW_MS),
                None => {
                    return Ok(Response::new(ListRefEventsResponse {
                        events: Vec::new(),
                        next_seq: req.after_seq,
                        truncated: false,
                    }));
                }
            },
            None => i64::MIN,
        };
        let scanned = self
            .meta
            .list_ref_events(req.after_seq, limit, repo)
            .await
            .map_err(status::from_store)?;
        // 跳过的旧事件也计入进度
        let last_scanned = scanned.last().map(|e| e.seq);
        let events: Vec<_> = scanned.into_iter().filter(|e| e.created_at_ms >= since).collect();
        let (oldest, last) = self.meta.ref_event_bounds().await.map_err(status::from_store)?;
        // after_seq 之后有过事件，但最早的那些已被清理
        let truncated = last > req.after_seq && oldest.is_none_or(|lo| lo > req.after_seq.saturating_add(1));
        Ok(Response::new(ListRefEventsResponse {
            next_seq: last_scanned.unwrap_or(req.after_seq),
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
        let gone = svc
            .list_ref_events(req(0, 0, "never-existed"))
            .await
            .expect("none")
            .into_inner();
        assert!(gone.events.is_empty() && gone.next_seq == 0);
        // 清理后落后的消费方得到 truncated
        store.prune_ref_events(i64::MAX).await.expect("prune");
        let behind = svc.list_ref_events(req(1, 0, "")).await.expect("behind").into_inner();
        assert!(behind.truncated && behind.events.is_empty());
        let current = svc.list_ref_events(req(3, 0, "")).await.expect("current").into_inner();
        assert!(!current.truncated);
    }

    /// 清除后重新创建的同名仓库：按仓库拉取看不到旧仓库的历史，管理员不带仓库拉取仍能看到。
    #[tokio::test]
    async fn a_recreated_repo_does_not_see_the_old_repos_events() {
        use scion_store::RepoInfo;
        let store = Arc::new(MemStore::new());
        let x = scion_object::ObjectId::hash(scion_object::ObjectKind::Changeset, b"x");
        let day = 24 * 3600 * 1000;
        // 旧仓库的事件发生在新仓库创建的一天之前
        store.create_repo_with(&RepoInfo::new("r", 0)).await.expect("old repo");
        store
            .cas_ref("r", "secret-branch", RefExpect::Absent, x)
            .await
            .expect("old ref");
        store.delete_repo("r", 1).await.expect("delete");
        while !store.purge_repo("r", 100).await.expect("purge") {}
        let now = store.list_ref_events(0, 1, None).await.expect("events")[0].created_at_ms;
        store
            .create_repo_with(&RepoInfo::new("r", now + day))
            .await
            .expect("recreate");
        let svc = EventSvc::new(store.clone());
        let page = svc.list_ref_events(req(0, 0, "r")).await.expect("list").into_inner();
        assert!(page.events.is_empty(), "old history hidden");
        assert_eq!(page.next_seq, 1, "progress still advances past the old events");
        let all = svc.list_ref_events(req(0, 0, "")).await.expect("all").into_inner();
        assert_eq!(all.events[0].r#ref, "secret-branch");
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
