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
    /// GitHub organizations whose members and teams become groups: `github:ORG` and
    /// `github:ORG/TEAM`.
    #[serde(default)]
    pub orgs: Vec<String>,
    /// The environment variable holding a token that can read those organizations' members and
    /// teams (a PAT or app token with `read:org`).
    pub token_env: Option<String>,
    /// How often the members are fetched again, such as `5m` (the default).
    #[serde(default, deserialize_with = "crate::config::duration_opt")]
    pub refresh: Option<std::time::Duration>,
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

/// Members of the configured GitHub organizations and their teams, as groups, kept in memory and
/// fetched again every `refresh` (in the background, serving the previous answer meanwhile). If a
/// fetch fails the previous groups stay: losing groups can only take permissions away.
pub struct Teams {
    state: std::sync::Arc<TeamsState>,
}

struct TeamsState {
    http: reqwest::Client,
    api_url: String,
    orgs: Vec<String>,
    token: String,
    prefix: String,
    refresh: std::time::Duration,
    /// login to groups, and when they were fetched.
    groups: std::sync::RwLock<Option<(LoginGroups, std::time::Instant)>>,
    fetching: tokio::sync::Mutex<()>,
}

/// login to its groups.
type LoginGroups = std::collections::HashMap<String, Vec<String>>;

/// GitHub pages lists by 100 at most.
const PAGE: usize = 100;

impl Teams {
    /// None when `[github]` names no organizations.
    pub fn new(
        config: &GitHubConfig,
        http: reqwest::Client,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        if config.orgs.is_empty() {
            return Ok(None);
        }
        let env = config
            .token_env
            .as_deref()
            .ok_or("[github] orgs needs token_env: a token that can read their members and teams")?;
        let token = lookup(env).ok_or_else(|| format!("environment variable {env} is not set"))?;
        Ok(Some(Self {
            state: std::sync::Arc::new(TeamsState {
                http,
                api_url: config.api_url.trim_end_matches('/').to_owned(),
                orgs: config.orgs.clone(),
                token,
                prefix: config.subject_prefix.clone(),
                refresh: config.refresh.unwrap_or(std::time::Duration::from_secs(300)),
                groups: std::sync::RwLock::new(None),
                fetching: tokio::sync::Mutex::new(()),
            }),
        }))
    }

    /// The groups of the GitHub user behind `subject` (the login with the subject prefix).
    pub async fn groups(&self, subject: &str) -> Vec<String> {
        let Some(login) = subject.strip_prefix(self.state.prefix.as_str()) else {
            return Vec::new();
        };
        let fresh = self.state.read(login);
        match fresh {
            Some((groups, stale)) => {
                if stale {
                    let state = self.state.clone();
                    tokio::spawn(async move { state.fetch().await });
                }
                groups
            }
            None => {
                self.state.fetch().await;
                self.state.read(login).map(|(g, _)| g).unwrap_or_default()
            }
        }
    }
}

impl TeamsState {
    /// The login's groups and whether they are due for a refresh; None before the first fetch.
    fn read(&self, login: &str) -> Option<(Vec<String>, bool)> {
        let guard = self.groups.read().ok()?;
        let (map, at) = guard.as_ref()?;
        let key = login.to_ascii_lowercase();
        Some((map.get(&key).cloned().unwrap_or_default(), at.elapsed() >= self.refresh))
    }

    async fn fetch(&self) {
        // One fetch at a time; a caller that waited finds it done
        let _one = self.fetching.lock().await;
        if self
            .groups
            .read()
            .is_ok_and(|g| g.as_ref().is_some_and(|(_, at)| at.elapsed() < self.refresh))
        {
            return;
        }
        match self.load().await {
            Ok(map) => {
                if let Ok(mut g) = self.groups.write() {
                    *g = Some((map, std::time::Instant::now()));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetching GitHub teams failed; keeping the previous groups");
                // Try again after another interval rather than on every request
                if let Ok(mut g) = self.groups.write() {
                    let map = g.take().map(|(m, _)| m).unwrap_or_default();
                    *g = Some((map, std::time::Instant::now()));
                }
            }
        }
    }

    async fn load(&self) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
        let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for org in &self.orgs {
            for login in self.logins(&format!("/orgs/{org}/members")).await? {
                map.entry(login).or_default().push(format!("github:{org}"));
            }
            for team in self.list(&format!("/orgs/{org}/teams")).await? {
                let slug = team
                    .get("slug")
                    .and_then(Value::as_str)
                    .ok_or("a team without a slug")?
                    .to_owned();
                for login in self.logins(&format!("/orgs/{org}/teams/{slug}/members")).await? {
                    map.entry(login).or_default().push(format!("github:{org}/{slug}"));
                }
            }
        }
        Ok(map)
    }

    async fn logins(&self, path: &str) -> Result<Vec<String>, String> {
        Ok(self
            .list(path)
            .await?
            .iter()
            .filter_map(|u| u.get("login").and_then(Value::as_str))
            .map(str::to_ascii_lowercase)
            .collect())
    }

    /// Every page of a list.
    async fn list(&self, path: &str) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        for page in 1.. {
            let resp = self
                .http
                .get(format!("{}{path}?per_page={PAGE}&page={page}", self.api_url))
                .bearer_auth(&self.token)
                .header("accept", "application/vnd.github+json")
                .header("user-agent", USER_AGENT)
                .send()
                .await
                .map_err(|e| format!("GitHub {path}: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("GitHub {path}: HTTP {}", resp.status()));
            }
            let items: Vec<Value> = resp.json().await.map_err(|e| format!("GitHub {path}: {e}"))?;
            let last = items.len() < PAGE;
            out.extend(items);
            if last {
                break;
            }
        }
        Ok(out)
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
                orgs: Vec::new(),
                token_env: None,
                refresh: None,
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

    fn teams(idp: &FakeIdp, orgs: &[&str], token: Option<&str>, prefix: &str) -> Result<Option<Teams>, String> {
        let config = GitHubConfig {
            subject_prefix: prefix.into(),
            orgs: orgs.iter().map(|o| (*o).to_owned()).collect(),
            token_env: token.map(|_| "GITHUB_TOKEN".to_owned()),
            refresh: Some(std::time::Duration::from_millis(200)),
            ..github(idp, "").config
        };
        let token = token.map(str::to_owned);
        Teams::new(&config, reqwest::Client::new(), &move |k: &str| {
            (k == "GITHUB_TOKEN").then(|| token.clone()).flatten()
        })
    }

    #[tokio::test]
    async fn teams_become_groups() {
        let idp = FakeIdp::start().await;
        assert!(teams(&idp, &[], None, "").expect("ok").is_none(), "no orgs, no teams");
        assert!(teams(&idp, &["opencsg"], None, "").is_err(), "token_env is required");
        let t = teams(&idp, &["opencsg", "big"], Some("ght"), "gh-")
            .expect("ok")
            .expect("teams");
        let mut g = t.groups("gh-octocat").await;
        g.sort();
        assert_eq!(
            g,
            ["github:opencsg", "github:opencsg/core"],
            "logins compare case-insensitively"
        );
        assert_eq!(t.groups("gh-u149").await, ["github:big"], "every page is read");
        assert!(t.groups("gh-nobody").await.is_empty());
        assert!(t.groups("octocat").await.is_empty(), "no prefix, not a GitHub subject");
        let calls = idp.calls("/gh/orgs/opencsg/members");
        assert_eq!(calls, 1, "fetched once");
        // Stale: the answer comes at once, and a refresh runs in the background
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(t.groups("gh-alice").await, ["github:opencsg"]);
        for _ in 0..50 {
            if idp.calls("/gh/orgs/opencsg/members") > calls {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(idp.calls("/gh/orgs/opencsg/members"), calls + 1, "refreshed");
        // A failing fetch leaves no groups (and is not retried on every request)
        let bad = teams(&idp, &["opencsg"], Some("wrong"), "")
            .expect("ok")
            .expect("teams");
        assert!(bad.groups("octocat").await.is_empty());
        assert!(bad.groups("octocat").await.is_empty());
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
                orgs: Vec::new(),
                token_env: None,
                refresh: None,
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
        let with_refresh: GitHubConfig = toml::from_str("client_id = \"c\"\nrefresh = \"10m\"").expect("parse");
        assert_eq!(with_refresh.refresh, Some(std::time::Duration::from_secs(600)));
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
