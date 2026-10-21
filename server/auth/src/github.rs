//! GitHub as an identity provider (docs/design/authorization.md): `scn login` runs GitHub's OAuth
//! device flow through the server, which reads `GET /user` with the resulting token and uses the
//! GitHub login as the subject. GitHub's token is not kept; the server issues its own tokens.

use serde::Deserialize;
use serde_json::Value;

use crate::external::VerifyError;
use crate::oidc::{DevicePoll, DeviceStart};
use crate::policy::Principal;

/// `[github]` in auth.toml.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    /// The OAuth app's client id (with the device flow enabled in its settings).
    pub client_id: String,
    /// Put before the GitHub login to make the subject, such as `gh-`; none by default.
    #[serde(default)]
    pub subject_prefix: String,
    /// Scopes to ask for; none are needed to read the login.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// `https://github.com`, or a GitHub Enterprise server.
    #[serde(default = "default_web_url")]
    pub web_url: String,
    /// `https://api.github.com`, or `https://HOST/api/v3` for GitHub Enterprise.
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

fn default_web_url() -> String {
    "https://github.com".into()
}

fn default_api_url() -> String {
    "https://api.github.com".into()
}

/// GitHub's API requires a User-Agent.
pub(crate) const USER_AGENT: &str = concat!("scion/", env!("CARGO_PKG_VERSION"));

pub struct GitHub {
    config: GitHubConfig,
    http: reqwest::Client,
}

impl GitHub {
    pub fn new(config: GitHubConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    fn web(&self, path: &str) -> String {
        format!("{}{path}", self.config.web_url.trim_end_matches('/'))
    }

    pub async fn start(&self) -> Result<DeviceStart, VerifyError> {
        let scope = self.config.scopes.join(" ");
        self.http
            .post(self.web("/login/device/code"))
            .header("accept", "application/json")
            .header("user-agent", USER_AGENT)
            .form(&[("client_id", self.config.client_id.as_str()), ("scope", scope.as_str())])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| VerifyError::Unavailable(format!("GitHub device code: {e}")))?
            .json()
            .await
            .map_err(|e| VerifyError::Unavailable(format!("GitHub device code response: {e}")))
    }

    pub async fn poll(&self, device_code: &str) -> Result<DevicePoll, VerifyError> {
        let body: Value = self
            .http
            .post(self.web("/login/oauth/access_token"))
            .header("accept", "application/json")
            .header("user-agent", USER_AGENT)
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| VerifyError::Unavailable(format!("GitHub access token: {e}")))?
            .json()
            .await
            .map_err(|e| VerifyError::Unavailable(format!("GitHub access token response: {e}")))?;
        // GitHub answers 200 with an error field while the user has not finished
        if let Some(e) = body.get("error").and_then(Value::as_str) {
            return Ok(match e {
                "authorization_pending" => DevicePoll::Pending,
                "slow_down" => DevicePoll::SlowDown,
                "access_denied" => DevicePoll::Denied,
                "expired_token" => DevicePoll::Expired,
                other => return Err(VerifyError::Invalid(format!("GitHub device flow error {other:?}"))),
            });
        }
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| VerifyError::Invalid("GitHub response has no access_token".into()))?;
        let login = self.login(token).await?;
        Ok(DevicePoll::Complete(Principal::user(
            format!("{}{login}", self.config.subject_prefix),
            Vec::new(),
        )))
    }

    /// The login of the user a token belongs to (`GET /user`).
    async fn login(&self, token: &str) -> Result<String, VerifyError> {
        let resp = self
            .http
            .get(format!("{}/user", self.config.api_url.trim_end_matches('/')))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| VerifyError::Unavailable(format!("GitHub /user: {e}")))?;
        if !resp.status().is_success() {
            return Err(VerifyError::Invalid(format!("GitHub /user: HTTP {}", resp.status())));
        }
        let user: Value = resp
            .json()
            .await
            .map_err(|e| VerifyError::Unavailable(format!("GitHub /user response: {e}")))?;
        user.get("login")
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| VerifyError::Invalid("GitHub /user has no login".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeIdp;

    fn github(idp: &FakeIdp, prefix: &str) -> GitHub {
        GitHub::new(
            GitHubConfig {
                client_id: "gh-app".into(),
                subject_prefix: prefix.into(),
                scopes: vec!["read:user".into()],
                web_url: idp.url("/gh"),
                api_url: idp.url("/gh"),
            },
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn device_flow_reads_the_login() {
        let idp = FakeIdp::start().await;
        let g = github(&idp, "gh-");
        let s = g.start().await.expect("start");
        assert_eq!(s.user_code, "WXYZ-1234");
        assert_eq!(g.poll(&s.device_code).await, Ok(DevicePoll::Pending));
        assert_eq!(g.poll("slow").await, Ok(DevicePoll::SlowDown));
        assert_eq!(g.poll("denied").await, Ok(DevicePoll::Denied));
        assert_eq!(g.poll("expired").await, Ok(DevicePoll::Expired));
        assert!(matches!(g.poll("weird").await, Err(VerifyError::Invalid(_))));
        assert!(matches!(g.poll("no-token").await, Err(VerifyError::Invalid(_))));
        assert!(
            matches!(g.poll("bad-token").await, Err(VerifyError::Invalid(_))),
            "/user refuses"
        );
        assert!(matches!(g.poll("no-login").await, Err(VerifyError::Invalid(_))));
        idp.approve(&s.device_code);
        match g.poll(&s.device_code).await.expect("poll") {
            DevicePoll::Complete(p) => assert_eq!((p.subject.as_str(), p.authenticated), ("gh-octocat", true)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_or_broken_github() {
        let idp = FakeIdp::start().await;
        let down = GitHub::new(
            GitHubConfig {
                client_id: "x".into(),
                subject_prefix: String::new(),
                scopes: Vec::new(),
                web_url: "http://127.0.0.1:1".into(),
                api_url: "http://127.0.0.1:1".into(),
            },
            reqwest::Client::new(),
        );
        assert!(matches!(down.start().await, Err(VerifyError::Unavailable(_))));
        assert!(matches!(down.poll("x").await, Err(VerifyError::Unavailable(_))));
        assert!(matches!(down.login("t").await, Err(VerifyError::Unavailable(_))));
        let broken = GitHub::new(
            GitHubConfig {
                web_url: idp.url("/not-json"),
                api_url: idp.url("/not-json"),
                ..github(&idp, "").config
            },
            reqwest::Client::new(),
        );
        assert!(broken.start().await.is_err());
        let config: GitHubConfig = toml::from_str("client_id = \"c\"").expect("parse");
        assert_eq!(
            (
                config.web_url.as_str(),
                config.api_url.as_str(),
                config.subject_prefix.as_str()
            ),
            ("https://github.com", "https://api.github.com", "")
        );
    }
}
