//! 仓库与分支管理端到端：经 SDK 创建、提交、fork、建删分支、删除与恢复，以及各操作的权限。

use std::sync::Arc;
use std::time::Duration;

use scion_api::scion_auth::{AuthConfig, Guard, GuardConfig};
use scion_api::{App, ServerConfig, serve};
use scion_client::{Auth, Client, Code, NewRepo};
use scion_store::{MemStore, MetaStore};
use tokio::net::TcpListener;

const POLICY: &str = r#"
[[rule]]
subjects = ["user:root"]
repos = ["*"]
actions = ["admin"]

[[rule]]
subjects = ["user:alice"]
repos = ["alice/*"]
actions = ["admin"]

[[rule]]
subjects = ["user:alice"]
repos = ["org/*"]
actions = ["read"]

[[rule]]
subjects = ["user:dev"]
repos = ["org/*"]
actions = ["land"]
"#;

struct Server {
    url: String,
    store: Arc<MemStore>,
    guard: Arc<Guard>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

async fn start() -> Server {
    let store = Arc::new(MemStore::new());
    let auth = GuardConfig {
        required: true,
        auth: AuthConfig::default(),
        signing_key_pem: None,
    };
    let guard = Guard::build(auth.clone(), store.clone(), Arc::new(|_: &str| None)).expect("guard");
    guard.set_policy(POLICY).await.expect("policy");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let app = App {
        config: Arc::new(ServerConfig {
            auth,
            warm_record_cache: false,
            janitor_interval: Duration::from_secs(3600),
            ..ServerConfig::default()
        }),
        meta: store.clone(),
        blobs: store.clone(),
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, app, async {
        let _ = rx.await;
    }));
    Server {
        url,
        store,
        guard,
        _stop: tx,
    }
}

async fn client(s: &Server, subject: &str, repo: &str) -> Client {
    let token = s
        .guard
        .create_token("pat", subject, vec![], "test", None)
        .await
        .expect("token")
        .0;
    Client::builder()
        .endpoint(&s.url)
        .repo(repo)
        .auth(Auth::Bearer(token))
        .build()
        .await
        .expect("connect")
}

fn code(e: scion_client::Error) -> Option<Code> {
    match e {
        scion_client::Error::Rpc { code, .. } => Some(code),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repository_and_branch_lifecycle() {
    let s = start().await;
    let root = client(&s, "root", "org/model").await;
    assert!(root.has_feature(scion_proto::features::REPOS_ADMIN));
    assert!(root.has_feature(scion_proto::features::REFS_WRITE));

    // 创建并提交
    let created = root
        .create_repo(
            "org/model",
            NewRepo {
                description: "weights".into(),
                ..NewRepo::default()
            },
        )
        .await
        .expect("create");
    assert_eq!((created.default_branch.as_str(), created.deleted_at_ms), ("main", None));
    let dev = client(&s, "dev", "org/model").await;
    let cs = dev
        .changeset(None)
        .put("README.md", "hello")
        .create()
        .await
        .expect("commit");
    dev.land("main", &cs.id).await.expect("land");

    // 分支：dev 有 land 权限
    let (target, version) = dev.create_ref("feature/x", &cs.id).await.expect("branch");
    assert_eq!((target, version), (cs.id, 1));
    let next = dev
        .changeset(Some(cs.id))
        .put("README.md", "hello again")
        .create()
        .await
        .expect("commit 2");
    dev.update_ref("feature/x", &next.id, Some(1)).await.expect("move");
    let refs: Vec<String> = dev.list_refs().await.expect("refs").into_iter().map(|r| r.0).collect();
    assert_eq!(refs, ["feature/x", "main"]);
    assert_eq!(
        code(dev.delete_ref("main", None).await.expect_err("default")),
        Some(Code::FailedPrecondition)
    );
    dev.delete_ref("feature/x", Some(2)).await.expect("delete branch");

    // 权限：alice 只能读 org/*，可以在 alice/* 下创建；fork 需要来源读权限与目标 admin
    let alice = client(&s, "alice", "org/model").await;
    assert_eq!(
        alice.get_repo("org/model", false).await.expect("read").description,
        "weights"
    );
    let err = alice
        .create_repo("org/other", NewRepo::default())
        .await
        .expect_err("not admin");
    assert_eq!(code(err), Some(Code::PermissionDenied));
    assert_eq!(
        code(alice.create_ref("x", &cs.id).await.expect_err("no land")),
        Some(Code::PermissionDenied)
    );
    let fork = alice.fork_repo("org/model", "alice/model").await.expect("fork");
    assert_eq!(fork.forked_from.as_deref(), Some("org/model"));
    let mine = alice.with_repo("alice/model");
    assert_eq!(
        mine.read_file("main", "README.md")
            .await
            .expect("forked history")
            .as_ref(),
        b"hello"
    );
    let err = alice
        .fork_repo("org/model", "org/copy")
        .await
        .expect_err("target not admin");
    assert_eq!(code(err), Some(Code::PermissionDenied));
    let page = alice.list_repos("", None, 10, false).await.expect("list");
    assert_eq!(
        page.repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["alice/model", "org/model"]
    );

    // 修改属性、删除与恢复
    let updated = root
        .update_repo("org/model", Some("main"), Some("v2 weights"))
        .await
        .expect("update");
    assert_eq!(updated.description, "v2 weights");
    let deleted = root.delete_repo("org/model").await.expect("delete");
    let (d, p) = (
        deleted.deleted_at_ms.expect("deleted"),
        deleted.purge_after_ms.expect("purge"),
    );
    assert_eq!(p - d, 24 * 3600 * 1000);
    assert_eq!(code(dev.list_refs().await.expect_err("gone")), Some(Code::NotFound));
    let err = root
        .create_repo("org/model", NewRepo::default())
        .await
        .expect_err("pending purge");
    assert_eq!(err.reason(), Some(scion_proto::v1::ErrorReason::RepoDeleted));
    let listed = root.list_repos("org/", None, 10, true).await.expect("deleted list");
    assert_eq!(listed.repos.len(), 1);
    assert!(
        root.get_repo("org/model", true)
            .await
            .expect("include deleted")
            .deleted_at_ms
            .is_some()
    );
    root.restore_repo("org/model").await.expect("restore");
    assert_eq!(
        dev.read_file("main", "README.md").await.expect("restored").as_ref(),
        b"hello"
    );
    assert!(s.store.repo_exists("alice/model").await.expect("fork kept"));
    let err = root
        .create_repo("org/model", NewRepo::default())
        .await
        .expect_err("exists");
    assert_eq!(err.reason(), Some(scion_proto::v1::ErrorReason::RepoAlreadyExists));
}
