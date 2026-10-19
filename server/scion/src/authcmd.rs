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
