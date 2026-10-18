//! RepoService：仓库的创建、列出、修改、删除、恢复与 fork（docs/guide/repos.md）。
//!
//! 删除是软删除：之后按名字找不到这个仓库，数据保留宽限期（[`RepoSvc::with_delete_grace`]），期间可以恢复；
//! 宽限期过后 janitor 清除数据（[`purge_deleted_repos`]），名字才能重新使用。

use std::sync::Arc;
use std::time::Duration;

use scion_auth::{Action, Guard, Principal, Request as AuthRequest};
use scion_proto::v1::repo_service_server::RepoService;
use scion_proto::v1::{
    CreateRepoRequest, CreateRepoResponse, DeleteRepoRequest, DeleteRepoResponse, ForkRepoRequest, ForkRepoResponse,
    GetRepoRequest, GetRepoResponse, ListReposRequest, ListReposResponse, Repo, RestoreRepoRequest,
    RestoreRepoResponse, UpdateRepoRequest, UpdateRepoResponse,
};
use scion_store::{DEFAULT_BRANCH, MetaStore, RepoFilter, RepoInfo, RepoPatch};
use tonic::{Request, Response, Status};

use crate::auth::{authorize, principal, to_status};
use crate::status;

/// 删除后保留数据的默认时间。
pub const DEFAULT_DELETE_GRACE: Duration = Duration::from_secs(24 * 3600);
/// 宽限期下限：各层缓存（提交记录、可见性）按仓库名缓存最多 1 小时，名字重新使用前它们必须已经过期。
pub const MIN_DELETE_GRACE: Duration = Duration::from_secs(2 * 3600);

const DEFAULT_PAGE: u32 = 100;
const MAX_PAGE: u32 = 1000;

pub struct RepoSvc {
    meta: Arc<dyn MetaStore>,
    guard: Arc<Guard>,
    grace: Duration,
}

impl RepoSvc {
    pub fn new(meta: Arc<dyn MetaStore>) -> Self {
        Self {
            meta,
            guard: Guard::disabled(),
            grace: DEFAULT_DELETE_GRACE,
        }
    }

    pub fn with_guard(mut self, guard: Arc<Guard>) -> Self {
        self.guard = guard;
        self
    }

    /// 删除后保留数据的时间（不小于 [`MIN_DELETE_GRACE`]）。
    pub fn with_delete_grace(mut self, grace: Duration) -> Self {
        self.grace = grace.max(MIN_DELETE_GRACE);
        self
    }

    fn to_proto(&self, r: RepoInfo) -> Repo {
        let grace_ms = i64::try_from(self.grace.as_millis()).unwrap_or(i64::MAX);
        Repo {
            purge_after_ms: r.deleted_at_ms.map_or(0, |d| d.saturating_add(grace_ms)),
            deleted_at_ms: r.deleted_at_ms.unwrap_or(0),
            name: r.name,
            default_branch: r.default_branch,
            description: r.description,
            created_at_ms: r.created_at_ms,
            forked_from: r.forked_from.unwrap_or_default(),
        }
    }

    async fn require(&self, p: &Principal, repo: &str, action: Action) -> Result<(), Status> {
        authorize(&self.guard, p, AuthRequest::repo(repo, action)).await
    }

    /// 正常（未删除）的仓库。
    async fn live(&self, repo: &str) -> Result<RepoInfo, Status> {
        match self.meta.get_repo(repo).await.map_err(status::from_store)? {
            Some(r) if r.deleted_at_ms.is_none() => Ok(r),
            _ => Err(status::repo_not_found(repo)),
        }
    }

    /// 调用者能否看到这个仓库（列出时用）。
    async fn visible(&self, p: &Principal, repo: &str, deleted: bool) -> Result<bool, Status> {
        let action = if deleted { Action::Admin } else { Action::Read };
        self.guard.check_any(p, repo, action).await.map_err(to_status)
    }
}

pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// 默认分支名：空串为 main，其余按 ref 名规则校验。
fn branch(name: &str) -> Result<String, Status> {
    if name.is_empty() {
        return Ok(DEFAULT_BRANCH.to_owned());
    }
    scion_land::validate_ref_name(name)
        .map_err(|_| Status::invalid_argument(format!("invalid branch name {name:?}")))?;
    Ok(name.to_owned())
}

#[tonic::async_trait]
impl RepoService for RepoSvc {
    async fn create_repo(&self, request: Request<CreateRepoRequest>) -> Result<Response<CreateRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        self.require(&p, &req.name, Action::Admin).await?;
        let info = RepoInfo {
            default_branch: branch(&req.default_branch)?,
            description: req.description,
            ..RepoInfo::new(&req.name, now_ms())
        };
        self.meta.create_repo_with(&info).await.map_err(status::from_store)?;
        tracing::info!(repo = %req.name, by = %p.subject, "repository created");
        let repo = self.live(&req.name).await?;
        Ok(Response::new(CreateRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }

    async fn get_repo(&self, request: Request<GetRepoRequest>) -> Result<Response<GetRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        let repo = if req.include_deleted {
            self.require(&p, &req.name, Action::Admin).await?;
            self.meta
                .get_repo(&req.name)
                .await
                .map_err(status::from_store)?
                .ok_or_else(|| status::repo_not_found(&req.name))?
        } else {
            self.require(&p, &req.name, Action::Read).await?;
            self.live(&req.name).await?
        };
        Ok(Response::new(GetRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }

    async fn list_repos(&self, request: Request<ListReposRequest>) -> Result<Response<ListReposResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        let page = match req.page_size {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_PAGE),
        } as usize;
        let mut filter = RepoFilter {
            prefix: req.prefix,
            after: Some(req.page_token).filter(|t| !t.is_empty()),
            limit: page,
            deleted: req.deleted,
        };
        // 只返回调用者能看到的仓库：逐批扫描，直到凑满一页或扫完
        let mut repos = Vec::new();
        let mut next = String::new();
        loop {
            let batch = self.meta.list_repo_infos(&filter).await.map_err(status::from_store)?;
            let exhausted = batch.len() < filter.limit;
            let mut last = None;
            for r in batch {
                last = Some(r.name.clone());
                if self.visible(&p, &r.name, req.deleted).await? {
                    repos.push(self.to_proto(r));
                    if repos.len() == page {
                        break;
                    }
                }
            }
            if repos.len() == page {
                if let Some(l) = last {
                    // 恰好扫完时再翻一页会得到空页，可以接受
                    next = l;
                }
                break;
            }
            if exhausted {
                break;
            }
            filter.after = last;
        }
        Ok(Response::new(ListReposResponse {
            repos,
            next_page_token: next,
        }))
    }

    async fn update_repo(&self, request: Request<UpdateRepoRequest>) -> Result<Response<UpdateRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        self.require(&p, &req.name, Action::Admin).await?;
        let patch = RepoPatch {
            default_branch: req.default_branch.as_deref().map(branch).transpose()?,
            description: req.description,
        };
        let repo = self
            .meta
            .update_repo(&req.name, &patch)
            .await
            .map_err(status::from_store)?;
        Ok(Response::new(UpdateRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }

    async fn delete_repo(&self, request: Request<DeleteRepoRequest>) -> Result<Response<DeleteRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        self.require(&p, &req.name, Action::Admin).await?;
        self.meta
            .delete_repo(&req.name, now_ms())
            .await
            .map_err(status::from_store)?;
        tracing::warn!(repo = %req.name, by = %p.subject, "repository deleted");
        let repo = self
            .meta
            .get_repo(&req.name)
            .await
            .map_err(status::from_store)?
            .ok_or_else(|| status::repo_not_found(&req.name))?;
        Ok(Response::new(DeleteRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }

    async fn restore_repo(
        &self,
        request: Request<RestoreRepoRequest>,
    ) -> Result<Response<RestoreRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        self.require(&p, &req.name, Action::Admin).await?;
        self.meta.restore_repo(&req.name).await.map_err(status::from_store)?;
        tracing::warn!(repo = %req.name, by = %p.subject, "repository restored");
        let repo = self.live(&req.name).await?;
        Ok(Response::new(RestoreRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }

    async fn fork_repo(&self, request: Request<ForkRepoRequest>) -> Result<Response<ForkRepoResponse>, Status> {
        let p = principal(&request);
        let req = request.into_inner();
        self.require(&p, &req.source, Action::Read).await?;
        self.require(&p, &req.target, Action::Admin).await?;
        self.meta
            .fork_repo(&req.source, &req.target, now_ms())
            .await
            .map_err(status::from_store)?;
        tracing::info!(source = %req.source, target = %req.target, by = %p.subject, "repository forked");
        let repo = self.live(&req.target).await?;
        Ok(Response::new(ForkRepoResponse {
            repo: Some(self.to_proto(repo)),
        }))
    }
}

/// 一次清除最多删除的行数。
const PURGE_BATCH: usize = 10_000;
/// janitor 每轮最多执行的清除批数（其余留到下一轮）。
const PURGE_ROUNDS: usize = 1_000;

/// 清除删除超过 `grace` 的仓库，返回已全部清除的仓库数。多个节点同时执行是安全的（删除是幂等的）。
pub async fn purge_deleted_repos(meta: &dyn MetaStore, grace: Duration, now_ms: i64) -> scion_store::Result<usize> {
    let grace_ms = i64::try_from(grace.max(MIN_DELETE_GRACE).as_millis()).unwrap_or(i64::MAX);
    let deleted = meta
        .list_repo_infos(&RepoFilter {
            limit: 1000,
            deleted: true,
            ..RepoFilter::default()
        })
        .await?;
    let mut rounds = 0;
    let mut purged = 0;
    for r in deleted {
        let due = r.deleted_at_ms.is_some_and(|d| d.saturating_add(grace_ms) <= now_ms);
        if !due {
            continue;
        }
        loop {
            if rounds >= PURGE_ROUNDS {
                return Ok(purged);
            }
            rounds += 1;
            match meta.purge_repo(&r.name, PURGE_BATCH).await {
                Ok(true) => {
                    tracing::warn!(repo = %r.name, "deleted repository purged");
                    purged += 1;
                    break;
                }
                Ok(false) => {}
                // 期间被恢复或已被其它节点清除
                Err(scion_store::Error::RepoNotFound(_)) => break,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(purged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_store::MemStore;
    use tonic::Code;

    fn svc() -> (Arc<MemStore>, RepoSvc) {
        let s = Arc::new(MemStore::new());
        (s.clone(), RepoSvc::new(s))
    }

    #[tokio::test]
    async fn lifecycle() {
        let (store, s) = svc();
        let created = s
            .create_repo(Request::new(CreateRepoRequest {
                name: "org/model".into(),
                default_branch: String::new(),
                description: "weights".into(),
            }))
            .await
            .expect("create")
            .into_inner()
            .repo
            .expect("repo");
        assert_eq!(
            (created.default_branch.as_str(), created.description.as_str()),
            ("main", "weights")
        );
        assert_eq!((created.deleted_at_ms, created.purge_after_ms), (0, 0));
        let err = s
            .create_repo(Request::new(CreateRepoRequest {
                name: "org/model".into(),
                ..Default::default()
            }))
            .await
            .expect_err("exists");
        assert_eq!(err.code(), Code::AlreadyExists);
        let err = s
            .create_repo(Request::new(CreateRepoRequest {
                name: "org/x".into(),
                default_branch: "../bad".into(),
                ..Default::default()
            }))
            .await
            .expect_err("bad branch");
        assert_eq!(err.code(), Code::InvalidArgument);

        let updated = s
            .update_repo(Request::new(UpdateRepoRequest {
                name: "org/model".into(),
                default_branch: Some("dev".into()),
                description: None,
            }))
            .await
            .expect("update")
            .into_inner()
            .repo
            .expect("repo");
        assert_eq!(
            (updated.default_branch.as_str(), updated.description.as_str()),
            ("dev", "weights")
        );

        let got = |include_deleted| GetRepoRequest {
            name: "org/model".into(),
            include_deleted,
        };
        assert!(s.get_repo(Request::new(got(false))).await.is_ok());
        let deleted = s
            .delete_repo(Request::new(DeleteRepoRequest {
                name: "org/model".into(),
            }))
            .await
            .expect("delete")
            .into_inner()
            .repo
            .expect("repo");
        assert!(deleted.deleted_at_ms > 0);
        assert_eq!(deleted.purge_after_ms - deleted.deleted_at_ms, 24 * 3600 * 1000);
        assert_eq!(
            s.get_repo(Request::new(got(false))).await.expect_err("gone").code(),
            Code::NotFound
        );
        assert!(
            s.get_repo(Request::new(got(true)))
                .await
                .expect("deleted visible")
                .into_inner()
                .repo
                .is_some()
        );
        let err = s
            .create_repo(Request::new(CreateRepoRequest {
                name: "org/model".into(),
                ..Default::default()
            }))
            .await
            .expect_err("pending purge");
        assert_eq!(err.code(), Code::AlreadyExists);
        assert!(err.message().contains("pending purge"), "{err:?}");

        let restored = s
            .restore_repo(Request::new(RestoreRepoRequest {
                name: "org/model".into(),
            }))
            .await
            .expect("restore")
            .into_inner()
            .repo
            .expect("repo");
        assert_eq!((restored.name.as_str(), restored.deleted_at_ms), ("org/model", 0));
        assert_eq!(
            s.restore_repo(Request::new(RestoreRepoRequest {
                name: "org/model".into()
            }))
            .await
            .expect_err("not deleted")
            .code(),
            Code::NotFound
        );

        let fork = s
            .fork_repo(Request::new(ForkRepoRequest {
                source: "org/model".into(),
                target: "me/model".into(),
            }))
            .await
            .expect("fork")
            .into_inner()
            .repo
            .expect("repo");
        assert_eq!(
            (fork.forked_from.as_str(), fork.default_branch.as_str()),
            ("org/model", "dev")
        );
        assert!(store.repo_exists("me/model").await.expect("exists"));
    }

    #[tokio::test]
    async fn list_pages_and_filters_by_permission() {
        use scion_auth::GuardConfig;
        let store = Arc::new(MemStore::new());
        for n in ["a/1", "a/2", "a/3", "b/1", "secret/1"] {
            store.create_repo(n).await.expect("create");
        }
        let s = RepoSvc::new(store.clone());
        let list = |prefix: &str, token: &str, size: u32, deleted: bool| ListReposRequest {
            prefix: prefix.into(),
            page_token: token.into(),
            page_size: size,
            deleted,
        };
        let page = s
            .list_repos(Request::new(list("a/", "", 2, false)))
            .await
            .expect("p1")
            .into_inner();
        assert_eq!(
            page.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a/1", "a/2"]
        );
        assert_eq!(page.next_page_token, "a/2");
        let page = s
            .list_repos(Request::new(list("a/", &page.next_page_token, 2, false)))
            .await
            .expect("p2")
            .into_inner();
        assert_eq!(page.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["a/3"]);
        assert_eq!(page.next_page_token, "");
        let all = s
            .list_repos(Request::new(list("", "", 0, false)))
            .await
            .expect("all")
            .into_inner();
        assert_eq!(all.repos.len(), 5);

        // 只返回能读的仓库；跳过不可见的仓库继续凑满一页
        let guard = scion_auth::Guard::build(
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
                repos = ["a/*", "b/*"]
                actions = ["read"]
                "#,
            )
            .await
            .expect("policy");
        let s = RepoSvc::new(store.clone()).with_guard(guard);
        let mut req = Request::new(list("", "", 2, false));
        req.extensions_mut().insert(Principal::user("alice", vec![]));
        let page = s.list_repos(req).await.expect("alice").into_inner();
        assert_eq!(
            page.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a/1", "a/2"]
        );
        let mut req = Request::new(list("", "a/2", 2, false));
        req.extensions_mut().insert(Principal::user("alice", vec![]));
        let page = s.list_repos(req).await.expect("alice p2").into_inner();
        assert_eq!(
            page.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a/3", "b/1"]
        );
        let mut req = Request::new(list("", "b/1", 2, false));
        req.extensions_mut().insert(Principal::user("alice", vec![]));
        let page = s.list_repos(req).await.expect("alice p3").into_inner();
        assert!(page.repos.is_empty(), "secret/1 is hidden");
        // 已删除的仓库需要 admin
        store.delete_repo("a/1", 1).await.expect("delete");
        let mut req = Request::new(list("", "", 10, true));
        req.extensions_mut().insert(Principal::user("alice", vec![]));
        assert!(s.list_repos(req).await.expect("deleted").into_inner().repos.is_empty());
        // 没有身份：创建需要 admin
        let err = s
            .create_repo(Request::new(CreateRepoRequest {
                name: "z".into(),
                ..Default::default()
            }))
            .await
            .expect_err("anonymous");
        assert_eq!(err.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn purges_after_the_grace_period() {
        let store = MemStore::new();
        store.create_repo("old").await.expect("create");
        store.create_repo("recent").await.expect("create");
        store.create_repo("live").await.expect("create");
        let hour = 3600 * 1000;
        store.delete_repo("old", 0).await.expect("delete");
        store.delete_repo("recent", 100 * hour).await.expect("delete");
        let grace = Duration::from_secs(48 * 3600);
        assert_eq!(purge_deleted_repos(&store, grace, 49 * hour).await, Ok(1));
        assert_eq!(store.get_repo("old").await, Ok(None));
        assert!(store.get_repo("recent").await.expect("get").is_some());
        // 宽限期有下限
        assert_eq!(purge_deleted_repos(&store, Duration::ZERO, 101 * hour).await, Ok(0));
        assert_eq!(purge_deleted_repos(&store, Duration::ZERO, 102 * hour).await, Ok(1));
        assert!(store.repo_exists("live").await.expect("live"));
        assert_eq!(
            RepoSvc::new(Arc::new(MemStore::new()))
                .with_delete_grace(Duration::ZERO)
                .grace,
            MIN_DELETE_GRACE
        );
    }
}
