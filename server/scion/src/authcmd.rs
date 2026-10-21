//! 认证相关的命令行参数与 `scion admin` 子命令（令牌、策略、签名密钥）。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use scion_api::scion_auth::{AuthConfig, Guard, GuardConfig};
use scion_store::MetaStore;

/// Authentication options of `scion server`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct AuthArgs {
    /// Require authentication: requests without a valid token only reach repositories the policy opens to anonymous.
    #[arg(long, env = "SCION_AUTH_REQUIRED")]
    pub auth_required: bool,
    /// Authentication config (auth.toml: external issuers, introspection, authorization callback, OIDC); see docs/guide/auth.md.
    #[arg(long, value_name = "FILE", env = "SCION_AUTH_CONFIG")]
    pub auth_config: Option<PathBuf>,
    /// ES256 key that signs access tokens (PKCS#8 PEM, from `scion admin jwt-keygen`). The same on every node.
    #[arg(long, value_name = "FILE", env = "SCION_JWT_KEY_FILE")]
    pub jwt_key: Option<PathBuf>,
}

impl AuthArgs {
    pub fn guard_config(&self) -> anyhow::Result<GuardConfig> {
        let auth = match &self.auth_config {
            Some(p) => {
                let text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
                AuthConfig::parse(&text).with_context(|| format!("parse {}", p.display()))?
            }
            None => AuthConfig::default(),
        };
        let signing_key_pem = match &self.jwt_key {
            Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?),
            None => None,
        };
        Ok(GuardConfig {
            required: self.auth_required,
            auth,
            signing_key_pem,
        })
    }
}

/// The Guard for admin commands: reads and writes the metadata store directly, no signing key needed.
fn admin_guard(meta: Arc<dyn MetaStore>) -> anyhow::Result<Arc<Guard>> {
    Guard::build(
        GuardConfig {
            required: true,
            ..GuardConfig::default()
        },
        meta,
        Arc::new(|k: &str| std::env::var(k).ok()),
    )
    .map_err(anyhow::Error::msg)
}

#[derive(Debug, clap::Subcommand)]
pub enum TokenCmd {
    /// Create a PAT or service token (shown only this once).
    Create {
        #[arg(long)]
        subject: String,
        #[arg(long = "group", value_name = "NAME")]
        groups: Vec<String>,
        #[arg(long, default_value = "")]
        description: String,
        /// Lifetime, such as 90d or 12h; no expiry when omitted.
        #[arg(long, value_name = "DURATION", value_parser = scion_api::scion_auth::config::parse_duration)]
        ttl: Option<std::time::Duration>,
    },
    /// List tokens (without the tokens themselves).
    List {
        #[arg(long)]
        subject: Option<String>,
    },
    /// Revoke a token.
    Revoke { id: String },
}

#[derive(Debug, clap::Subcommand)]
pub enum PolicyCmd {
    /// Show the access control policy.
    Show,
    /// Check and set the access control policy (a TOML file). Running servers apply it within 30 seconds.
    Set { file: PathBuf },
}

pub async fn token(cmd: &TokenCmd, meta: Arc<dyn MetaStore>) -> anyhow::Result<Vec<String>> {
    let g = admin_guard(meta)?;
    Ok(match cmd {
        TokenCmd::Create {
            subject,
            groups,
            description,
            ttl,
        } => {
            let (token, rec) = g
                .create_token("pat", subject, groups.clone(), description, *ttl)
                .await
                .map_err(anyhow::Error::msg)?;
            vec![format!("id:    {}", rec.id), format!("token: {token}")]
        }
        TokenCmd::List { subject } => g
            .list_tokens(subject.as_deref())
            .await
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|t| {
                let expires = t.expires_at_ms.map_or_else(|| "never".to_string(), |e| e.to_string());
                format!(
                    "{}  {:<7} {:<20} groups={} expires={} {}",
                    t.id,
                    t.kind,
                    t.subject,
                    t.groups.join(","),
                    expires,
                    t.description
                )
            })
            .collect(),
        TokenCmd::Revoke { id } => {
            let revoked = g.revoke_token(id).await.map_err(anyhow::Error::msg)?;
            vec![if revoked {
                format!("revoked {id}")
            } else {
                format!("no token {id}")
            }]
        }
    })
}

#[derive(Debug, clap::Subcommand)]
pub enum OrgCmd {
    /// Create an organization with its first owner.
    Create {
        name: String,
        #[arg(long)]
        owner: String,
        /// private (only members read) or internal (every signed-in user reads).
        #[arg(long, default_value = "private")]
        visibility: String,
    },
    /// List organizations.
    Ls,
    /// Change who besides the members can read: private or internal.
    Visibility { name: String, visibility: String },
    /// Delete an organization (it must have no repositories left).
    Rm { name: String },
    /// Members: add (or change the role of), remove, list.
    #[command(subcommand)]
    Member(MemberCmd),
}

#[derive(Debug, clap::Subcommand)]
pub enum MemberCmd {
    /// Add a member, or change an existing member's role.
    Add {
        org: String,
        subject: String,
        /// owner, maintainer, developer or reporter.
        #[arg(long)]
        role: String,
    },
    Rm {
        org: String,
        subject: String,
    },
    Ls {
        org: String,
    },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn role(r: &str) -> anyhow::Result<&'static str> {
    scion_api::scion_auth::orgs::Role::parse(r)
        .map(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown role {r:?}; use owner, maintainer, developer or reporter"))
}

fn visibility(v: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        scion_api::scion_auth::orgs::VISIBILITIES.contains(&v),
        "unknown visibility {v:?}; use private or internal"
    );
    Ok(())
}

/// `scion admin org`: works on the metadata store directly, as an operator (no permission checks).
pub async fn org(cmd: &OrgCmd, meta: Arc<dyn MetaStore>) -> anyhow::Result<Vec<String>> {
    use scion_store::{OrgMember, OrgRecord, RepoFilter};
    const BY: &str = "scion admin";
    Ok(match cmd {
        OrgCmd::Create {
            name,
            owner,
            visibility: v,
        } => {
            scion_api::scion_auth::orgs::validate_org_name(name).map_err(anyhow::Error::msg)?;
            visibility(v)?;
            let now = now_ms();
            meta.create_org(&OrgRecord {
                name: name.clone(),
                visibility: v.clone(),
                created_at_ms: now,
                created_by: BY.into(),
            })
            .await?;
            meta.put_org_member(&OrgMember {
                org: name.clone(),
                subject: owner.clone(),
                role: "owner".into(),
                added_at_ms: now,
                added_by: BY.into(),
            })
            .await?;
            vec![format!("created {name} ({v}), owned by {owner}")]
        }
        OrgCmd::Ls => meta
            .list_orgs()
            .await?
            .into_iter()
            .map(|o| format!("{:<24} {:<9} created by {}", o.name, o.visibility, o.created_by))
            .collect(),
        OrgCmd::Visibility { name, visibility: v } => {
            visibility(v)?;
            anyhow::ensure!(meta.set_org_visibility(name, v).await?, "no organization {name}");
            vec![format!("{name} is now {v}")]
        }
        OrgCmd::Rm { name } => {
            for deleted in [false, true] {
                let filter = RepoFilter {
                    prefix: format!("{name}/"),
                    after: None,
                    limit: 1,
                    deleted,
                };
                anyhow::ensure!(
                    meta.list_repo_infos(&filter).await?.is_empty(),
                    "{name} still has repositories; delete them (and wait until they are purged) first"
                );
            }
            anyhow::ensure!(meta.delete_org(name).await?, "no organization {name}");
            vec![format!("deleted {name}")]
        }
        OrgCmd::Member(MemberCmd::Add { org, subject, role: r }) => {
            let r = role(r)?;
            meta.put_org_member(&OrgMember {
                org: org.clone(),
                subject: subject.clone(),
                role: r.into(),
                added_at_ms: now_ms(),
                added_by: BY.into(),
            })
            .await?;
            vec![format!("{subject} is {r} of {org}")]
        }
        OrgCmd::Member(MemberCmd::Rm { org, subject }) => {
            anyhow::ensure!(
                meta.delete_org_member(org, subject).await?,
                "{subject} is not a member of {org}"
            );
            vec![format!("removed {subject} from {org}")]
        }
        OrgCmd::Member(MemberCmd::Ls { org }) => {
            anyhow::ensure!(meta.get_org(org).await?.is_some(), "no organization {org}");
            meta.list_org_members(org)
                .await?
                .into_iter()
                .map(|m| format!("{:<24} {:<10} added by {}", m.subject, m.role, m.added_by))
                .collect()
        }
    })
}

pub async fn policy(cmd: &PolicyCmd, meta: Arc<dyn MetaStore>) -> anyhow::Result<Vec<String>> {
    let g = admin_guard(meta)?;
    Ok(match cmd {
        PolicyCmd::Show => vec![g.policy_text().await.map_err(anyhow::Error::msg)?],
        PolicyCmd::Set { file } => {
            let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
            let n = g.set_policy(&text).await.map_err(anyhow::Error::msg)?;
            vec![format!("policy set: {n} rules")]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_store::MemStore;

    #[tokio::test]
    async fn org_commands() {
        let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let run = |cmd: OrgCmd| {
            let meta = meta.clone();
            async move { org(&cmd, meta).await }
        };
        let create = |name: &str, visibility: &str| OrgCmd::Create {
            name: name.into(),
            owner: "alice".into(),
            visibility: visibility.into(),
        };
        assert_eq!(
            run(create("acme", "private")).await.expect("create"),
            ["created acme (private), owned by alice"]
        );
        assert!(run(create("acme", "private")).await.is_err(), "exists");
        assert!(run(create("user", "private")).await.is_err(), "reserved");
        assert!(run(create("x", "public")).await.is_err(), "visibility");
        let add = |subject: &str, role: &str| {
            OrgCmd::Member(MemberCmd::Add {
                org: "acme".into(),
                subject: subject.into(),
                role: role.into(),
            })
        };
        assert_eq!(
            run(add("bob", "developer")).await.expect("add"),
            ["bob is developer of acme"]
        );
        assert!(run(add("bob", "boss")).await.is_err());
        let members = run(OrgCmd::Member(MemberCmd::Ls { org: "acme".into() }))
            .await
            .expect("ls");
        assert!(
            members[0].starts_with("alice") && members[1].contains("developer"),
            "{members:?}"
        );
        assert!(run(OrgCmd::Member(MemberCmd::Ls { org: "nope".into() })).await.is_err());
        let rm = |subject: &str| {
            OrgCmd::Member(MemberCmd::Rm {
                org: "acme".into(),
                subject: subject.into(),
            })
        };
        assert_eq!(run(rm("bob")).await.expect("rm"), ["removed bob from acme"]);
        assert!(run(rm("bob")).await.is_err());
        let vis = |name: &str, v: &str| OrgCmd::Visibility {
            name: name.into(),
            visibility: v.into(),
        };
        assert_eq!(
            run(vis("acme", "internal")).await.expect("vis"),
            ["acme is now internal"]
        );
        assert!(run(vis("nope", "internal")).await.is_err());
        assert!(run(vis("acme", "secret")).await.is_err());
        assert!(run(OrgCmd::Ls).await.expect("ls")[0].contains("internal"));
        meta.create_repo("acme/app").await.expect("repo");
        assert!(
            run(OrgCmd::Rm { name: "acme".into() }).await.is_err(),
            "has repositories"
        );
        run(create("tmp", "private")).await.expect("create");
        assert_eq!(
            run(OrgCmd::Rm { name: "tmp".into() }).await.expect("rm"),
            ["deleted tmp"]
        );
        assert!(run(OrgCmd::Rm { name: "tmp".into() }).await.is_err());
    }

    #[tokio::test]
    async fn token_commands() {
        let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        let created = token(
            &TokenCmd::Create {
                subject: "ci".into(),
                groups: vec!["eng".into()],
                description: "pipeline".into(),
                ttl: Some(std::time::Duration::from_secs(3600)),
            },
            meta.clone(),
        )
        .await
        .expect("create");
        assert!(created[1].starts_with("token: scn_pat_"));
        let id = created[0].trim_start_matches("id:").trim().to_string();
        let listed = token(&TokenCmd::List { subject: None }, meta.clone())
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].contains("ci") && listed[0].contains("groups=eng") && listed[0].contains("pipeline"));
        let never = token(
            &TokenCmd::Create {
                subject: "x".into(),
                groups: vec![],
                description: String::new(),
                ttl: None,
            },
            meta.clone(),
        )
        .await
        .expect("create");
        assert_eq!(never.len(), 2);
        let listed = token(
            &TokenCmd::List {
                subject: Some("x".into()),
            },
            meta.clone(),
        )
        .await
        .expect("list");
        assert!(listed[0].contains("expires=never"));
        assert_eq!(
            token(&TokenCmd::Revoke { id: id.clone() }, meta.clone())
                .await
                .expect("revoke"),
            [format!("revoked {id}")]
        );
        assert_eq!(
            token(&TokenCmd::Revoke { id: id.clone() }, meta.clone())
                .await
                .expect("again"),
            [format!("no token {id}")]
        );
        let bad = token(
            &TokenCmd::Create {
                subject: " ".into(),
                groups: vec![],
                description: String::new(),
                ttl: None,
            },
            meta,
        )
        .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn policy_commands() {
        let meta: Arc<dyn MetaStore> = Arc::new(MemStore::new());
        assert_eq!(policy(&PolicyCmd::Show, meta.clone()).await.expect("show"), [""]);
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("acl.toml");
        std::fs::write(&f, "[[rule]]\nsubjects=[\"*\"]\nrepos=[\"*\"]\nactions=[\"read\"]\n").expect("write");
        assert_eq!(
            policy(&PolicyCmd::Set { file: f.clone() }, meta.clone())
                .await
                .expect("set"),
            ["policy set: 1 rules"]
        );
        assert!(policy(&PolicyCmd::Show, meta.clone()).await.expect("show")[0].contains("subjects"));
        std::fs::write(&f, "broken [").expect("write");
        assert!(policy(&PolicyCmd::Set { file: f }, meta.clone()).await.is_err());
        assert!(
            policy(
                &PolicyCmd::Set {
                    file: dir.path().join("missing")
                },
                meta
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn guard_config_from_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("auth.toml");
        std::fs::write(&cfg, "[token]\nissuer = \"https://s\"\n").expect("write");
        let key = dir.path().join("key.pem");
        std::fs::write(&key, scion_api::scion_auth::token::generate_signing_key()).expect("write");
        let a = AuthArgs {
            auth_required: true,
            auth_config: Some(cfg.clone()),
            jwt_key: Some(key),
        };
        let g = a.guard_config().expect("config");
        assert!(g.required && g.signing_key_pem.is_some());
        assert_eq!(g.auth.token.issuer, "https://s");
        assert_eq!(
            AuthArgs::default().guard_config().expect("default"),
            GuardConfig::default()
        );
        let missing = AuthArgs {
            auth_config: Some(dir.path().join("nope.toml")),
            ..AuthArgs::default()
        };
        assert!(missing.guard_config().is_err());
        std::fs::write(&cfg, "[nope").expect("write");
        assert!(
            AuthArgs {
                auth_config: Some(cfg),
                ..AuthArgs::default()
            }
            .guard_config()
            .is_err()
        );
        let no_key = AuthArgs {
            jwt_key: Some(dir.path().join("none.pem")),
            ..AuthArgs::default()
        };
        assert!(no_key.guard_config().is_err());
    }
}
