//! The authorization chain end to end (docs/design/authorization.md): one server whose namespaces
//! are decided by different deciders — a webhook for `models/*`, the policy (with deny rules and
//! static groups), organizations and personal namespaces.

use std::sync::Arc;

use scion_api::scion_auth::testing::FakeIdp;
use scion_api::scion_auth::{AuthConfig, Guard, GuardConfig};
use scion_api::{App, ServerConfig, serve};
use scion_client::{Auth, Client, NewRepo};
use scion_proto::v1::auth_service_client::AuthServiceClient;
use scion_proto::v1::{Action, CheckAccessRequest};
use scion_store::{MemStore, MetaStore, OrgMember, OrgRecord};
use tokio::net::TcpListener;
use tonic::metadata::MetadataValue;

const POLICY: &str = r#"
[groups]
ops = ["user:carol"]

[[rule]]
subjects = ["anonymous", "*"]
repos = ["public/*"]
actions = ["read"]

[[rule]]
subjects = ["group:ops"]
repos = ["infra/*"]
actions = ["land"]

[[rule]]
effect = "deny"
subjects = ["user:carl"]
repos = ["acme/*"]
paths = ["secrets"]
actions = ["read"]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn each_namespace_is_decided_by_its_decider() {
    let idp = FakeIdp::start().await;
    let auth_toml = format!(
        r#"
[authorization]
chain = ["webhook:models", "policy", "orgs", "personal"]

[[authorization.webhook]]
name = "models"
url = "{authz}"
bearer = "opa"
decides = "allow-deny"
repos = ["models/*"]
"#,
        authz = idp.url("/authz")
    );
    let store = Arc::new(MemStore::new());
    store
        .create_org(&OrgRecord {
            name: "acme".into(),
            visibility: "private".into(),
            created_at_ms: 0,
            created_by: "root".into(),
        })
        .await
        .expect("org");
    for (subject, role) in [("alice", "owner"), ("bob", "developer"), ("carl", "developer")] {
        store
            .put_org_member(&OrgMember {
                org: "acme".into(),
                subject: subject.into(),
                role: role.into(),
                added_at_ms: 0,
                added_by: "root".into(),
            })
            .await
            .expect("member");
    }
    let auth = GuardConfig {
        required: true,
        auth: AuthConfig::parse(&auth_toml).expect("auth config"),
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
            ..ServerConfig::default()
        }),
        meta: store.clone(),
        blobs: store,
    };
    tokio::spawn(serve(listener, app, std::future::pending()));

    let mut tokens = std::collections::HashMap::new();
    for who in ["alice", "bob", "carl", "carol", "dave"] {
        let t = guard.create_token("pat", who, vec![], "", None).await.expect("pat").0;
        tokens.insert(who, t);
    }
    let check = |who: &'static str, repo: &'static str, action: Action, path: &'static str| {
        let (url, token) = (url.clone(), tokens.get(who).cloned());
        async move {
            let mut a = AuthServiceClient::connect(url).await.expect("connect");
            let mut req = tonic::Request::new(CheckAccessRequest {
                repo: repo.into(),
                action: action as i32,
                r#ref: String::new(),
                path: path.into(),
            });
            if let Some(t) = token {
                req.metadata_mut().insert(
                    "authorization",
                    MetadataValue::try_from(format!("Bearer {t}")).expect("header"),
                );
            }
            a.check_access(req).await.expect("check").into_inner().allowed
        }
    };

    // models/*: the webhook decides, and its "no" stands
    assert!(check("alice", "models/a", Action::Read, "").await);
    assert!(!check("alice", "models/a", Action::Write, "").await);
    // public/*: the policy, also for anonymous callers
    assert!(check("anonymous", "public/docs", Action::Read, "").await);
    assert!(!check("anonymous", "public/docs", Action::Write, "").await);
    // infra/*: a policy group defined in the policy itself
    assert!(check("carol", "infra/deploy", Action::Land, "").await);
    assert!(!check("bob", "infra/deploy", Action::Read, "").await);
    // acme/*: organization roles, with the policy's deny rule first in line
    assert!(check("alice", "acme/app", Action::Admin, "").await);
    assert!(check("bob", "acme/app", Action::Write, "").await);
    assert!(!check("bob", "acme/app", Action::Land, "").await);
    assert!(check("carl", "acme/app", Action::Read, "src/main.rs").await);
    assert!(
        !check("carl", "acme/app", Action::Read, "secrets/key").await,
        "denied by the policy"
    );
    assert!(!check("carol", "acme/app", Action::Read, "").await, "not a member");
    // user/*: personal namespaces
    assert!(check("dave", "user/dave/notes", Action::Admin, "").await);
    assert!(!check("dave", "user/alice/notes", Action::Read, "").await);
    // Nobody allows everything else
    assert!(!check("alice", "elsewhere", Action::Read, "").await);

    // The decisions hold for real calls too
    let as_user = |who: &'static str| {
        let (url, token) = (url.clone(), tokens[who].clone());
        async move {
            Client::builder()
                .endpoint(&url)
                .auth(Auth::Bearer(token))
                .build()
                .await
                .expect("connect")
        }
    };
    as_user("alice")
        .await
        .create_repo("acme/app", NewRepo::default())
        .await
        .expect("owner");
    as_user("dave")
        .await
        .create_repo("user/dave/notes", NewRepo::default())
        .await
        .expect("personal");
    assert!(
        as_user("bob")
            .await
            .create_repo("acme/other", NewRepo::default())
            .await
            .is_err()
    );
    assert!(
        as_user("dave")
            .await
            .create_repo("user/alice/x", NewRepo::default())
            .await
            .is_err()
    );
    let listed = as_user("bob")
        .await
        .list_repos("", None, 100, false)
        .await
        .expect("list");
    let names: Vec<String> = listed.repos.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["acme/app"], "bob sees only what he can read");
}
