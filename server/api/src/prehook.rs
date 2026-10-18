//! 合入前检查（docs/guide/events.md#合入前检查）：合入、创建或移动 ref 之前，把要引入的提交与路径级改动
//! POST 给外部服务，由它决定是否允许（例如 starhub 的内容审查、文件类型限制）。
//!
//! - 2xx：允许；响应体为 `{"allowed": false, "message": "…"}` 时拒绝；
//!   请求被截断（提交或路径超过上限）时，只有响应明确 `{"accept_truncated": true}` 才放行，
//!   否则拒绝并让客户端拆小后重试：检查服务没看到的提交和路径不能算通过；
//! - 4xx（408、429 除外）：拒绝，响应体作为原因；
//! - 其它状态、超时、连接失败：检查服务不可用，按 `fail_open` 放行或拒绝（默认拒绝）。

use std::sync::Arc;
use std::time::Duration;

use scion_auth::Principal;
use scion_object::ObjectId;
use scion_store::{BlobStore, MetaStore};
use serde::{Deserialize, Serialize};
use tonic::Status;

use crate::events::{CommitInfo, MAX_COMMITS, commit_info, signature};
use crate::status;

/// 原因最多保留的字节数。
const MAX_MESSAGE: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreLandConfig {
    pub url: String,
    /// HMAC-SHA256 签名密钥（`X-Scion-Signature-256`）。
    pub secret: Option<String>,
    pub timeout: Duration,
    /// 检查服务不可用时放行（默认拒绝）。
    pub fail_open: bool,
}

#[derive(Debug, Serialize)]
struct Pusher {
    subject: String,
    groups: Vec<String>,
    authenticated: bool,
}

#[derive(Debug, Serialize)]
struct CheckRequest {
    #[serde(rename = "type")]
    kind: &'static str,
    repo: String,
    #[serde(rename = "ref")]
    reference: String,
    /// ref 当前的提交；为空表示 ref 还不存在。
    old: Option<String>,
    /// 要合入（或指向）的提交。
    new: String,
    pusher: Pusher,
    commits: Vec<CommitInfo>,
    commits_truncated: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CheckResponse {
    allowed: Option<bool>,
    message: Option<String>,
    /// 请求被截断时，检查服务明确接受只看到部分内容的结果。
    accept_truncated: Option<bool>,
}

/// 请求被截断、检查服务没有明确接受时的拒绝原因。
pub const TRUNCATED_MESSAGE: &str = "the change is too large for the pre-land check (at most 100 commits and 1000 \
paths per commit are sent); land it in smaller pieces";

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

pub struct PreLand {
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
    /// `None`：客户端无法创建，所有检查都视为检查服务不可用。
    client: Option<reqwest::Client>,
    cfg: PreLandConfig,
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// 按 HTTP 状态和响应体判断（`truncated`：请求中的提交或路径被截断）。`Err` 表示检查服务不可用。
pub fn verdict(status: u16, body: &[u8], truncated: bool) -> Result<Verdict, String> {
    let text = || truncate(String::from_utf8_lossy(body).trim().to_owned());
    match status {
        200..=299 => {
            let r: CheckResponse = serde_json::from_slice(body).unwrap_or_default();
            Ok(match r.allowed {
                Some(false) => Verdict::Deny(truncate(
                    r.message.unwrap_or_else(|| "rejected by the pre-land check".into()),
                )),
                _ if truncated && r.accept_truncated != Some(true) => Verdict::Deny(TRUNCATED_MESSAGE.into()),
                _ => Verdict::Allow,
            })
        }
        408 | 429 => Err(format!("HTTP {status}")),
        400..=499 => {
            let m = text();
            Ok(Verdict::Deny(if m.is_empty() {
                "rejected by the pre-land check".into()
            } else {
                m
            }))
        }
        _ => Err(format!("HTTP {status}")),
    }
}

impl PreLand {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>, cfg: PreLandConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("pre-land client: {e}"))?;
        Ok(Self {
            meta,
            blobs,
            client: Some(client),
            cfg,
        })
    }

    /// 配置了检查却无法创建客户端时使用：一律拒绝（不放行，即使配置了 fail_open）。
    pub fn refuse_all(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>, cfg: PreLandConfig) -> Self {
        Self {
            meta,
            blobs,
            client: None,
            cfg: PreLandConfig {
                fail_open: false,
                ..cfg
            },
        }
    }

    /// 要引入的提交（ref 中还没有的）。
    async fn request(
        &self,
        p: &Principal,
        repo: &str,
        reference: &str,
        tip: ObjectId,
    ) -> Result<Option<(Vec<u8>, bool)>, String> {
        let old = self
            .meta
            .get_ref(repo, reference)
            .await
            .map_err(|e| e.to_string())?
            .map(|s| s.target);
        // 新建的 ref 以默认分支为基准：默认分支上的内容合入时已经检查过
        let base = match old {
            Some(o) => Some(o),
            None => self.default_tip(repo, reference).await?,
        };
        let (recs, truncated) = scion_query::new_commits(self.meta.as_ref(), repo, base, tip, MAX_COMMITS)
            .await
            .map_err(|e| e.to_string())?;
        if recs.is_empty() {
            // 没有新内容（已经合入过，或回退到旧提交）
            return Ok(None);
        }
        let mut commits = Vec::with_capacity(recs.len());
        for r in recs {
            let cs = scion_query::load_changeset(self.blobs.as_ref(), r.id)
                .await
                .map_err(|e| e.to_string())?;
            commits.push(commit_info(r.id, &cs));
        }
        let truncated = truncated || commits.iter().any(|c| c.changes_truncated);
        let req = CheckRequest {
            kind: "pre-land",
            repo: repo.to_owned(),
            reference: reference.to_owned(),
            old: old.map(|o| o.to_string()),
            new: tip.to_string(),
            pusher: Pusher {
                subject: p.subject.clone(),
                groups: p.groups.clone(),
                authenticated: p.authenticated,
            },
            commits,
            commits_truncated: truncated,
        };
        serde_json::to_vec(&req)
            .map(|b| Some((b, truncated)))
            .map_err(|e| e.to_string())
    }

    /// 默认分支的当前提交（`reference` 本身就是默认分支，或默认分支还不存在时为 `None`）。
    async fn default_tip(&self, repo: &str, reference: &str) -> Result<Option<ObjectId>, String> {
        let Some(info) = self.meta.get_repo(repo).await.map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        if info.default_branch == reference {
            return Ok(None);
        }
        Ok(self
            .meta
            .get_ref(repo, &info.default_branch)
            .await
            .map_err(|e| e.to_string())?
            .map(|s| s.target))
    }

    async fn ask(&self, body: Vec<u8>, truncated: bool) -> Result<Verdict, String> {
        let Some(client) = &self.client else {
            return Err("pre-land client unavailable".into());
        };
        let mut req = client
            .post(&self.cfg.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Scion-Event", "pre-land");
        if let Some(secret) = &self.cfg.secret {
            req = req.header("X-Scion-Signature-256", signature(secret.as_bytes(), &body));
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| format!("POST {}: {e}", self.cfg.url))?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        verdict(status, &bytes, truncated)
    }

    /// 检查一次合入或 ref 修改；拒绝时返回 PRE_LAND_REJECTED，检查服务不可用且不放行时返回 UNAVAILABLE。
    pub async fn check(&self, p: &Principal, repo: &str, reference: &str, tip: ObjectId) -> Result<(), Status> {
        let outcome = match self.request(p, repo, reference, tip).await {
            Ok(None) => return Ok(()),
            Ok(Some((body, truncated))) => self.ask(body, truncated).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(Verdict::Allow) => {
                scion_obs::metrics().pre_land("allowed");
                Ok(())
            }
            Ok(Verdict::Deny(message)) => {
                scion_obs::metrics().pre_land("denied");
                tracing::info!(%repo, r#ref = %reference, %tip, by = %p.subject, %message, "pre-land check rejected");
                Err(status::pre_land_rejected(reference, &message))
            }
            Err(e) => {
                scion_obs::metrics().pre_land("error");
                if self.cfg.fail_open {
                    tracing::warn!(%repo, r#ref = %reference, error = %e, "pre-land check unavailable; allowing");
                    Ok(())
                } else {
                    tracing::warn!(%repo, r#ref = %reference, error = %e, "pre-land check unavailable; refusing");
                    Err(Status::unavailable(format!("pre-land check unavailable: {e}")))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use scion_store::{MemStore, RefExpect};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 本地检查服务：按顺序回复给定的 (状态码, 响应体)，记录收到的请求（请求头, 请求体）。
    type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    async fn hook(replies: Vec<(u16, &'static str)>) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/check", listener.local_addr().expect("addr"));
        let seen: Seen = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, req) = loop {
                    let n = s.read(&mut chunk).await.expect("read");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = s.read(&mut chunk).await.expect("body");
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[i + 4..i + 4 + len].to_vec());
                    }
                };
                log.lock()
                    .expect("lock")
                    .push((head, serde_json::from_slice(&req).expect("json")));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        (url, seen)
    }

    async fn commit(store: &MemStore, parents: Vec<ObjectId>, path: &str) -> ObjectId {
        use scion_object::{Changeset, EntryMode, FileChange, Signature};
        let who = Signature {
            name: "Ada".into(),
            email: "ada@example.com".into(),
            time_ms: 5,
            tz_offset_minutes: 0,
        };
        let cs = Changeset::new(
            parents.clone(),
            who.clone(),
            who,
            "msg",
            [path.len() as u8; 16],
            vec![FileChange::put(
                path,
                EntryMode::File,
                ObjectId::of_blob(path.as_bytes()),
                4,
            )],
            Default::default(),
        )
        .expect("changeset");
        let bytes = cs.encode();
        let id = ObjectId::hash(scion_object::ObjectKind::Changeset, &bytes);
        store.put(id, bytes::Bytes::from(bytes)).await.expect("put");
        let generation = parents.len() as u64 + 1;
        let rec = scion_store::ChangesetRecord {
            id,
            root_tree: ObjectId::hash(scion_object::ObjectKind::Tree, path.as_bytes()),
            parents,
            generation,
            fp_depth: generation,
            fp_jump: None,
        };
        store.put_changeset("r", &rec).await.expect("record");
        id
    }

    fn pre(store: &Arc<MemStore>, url: &str, fail_open: bool) -> PreLand {
        PreLand::new(
            store.clone(),
            store.clone(),
            PreLandConfig {
                url: url.into(),
                secret: Some("k".into()),
                timeout: Duration::from_secs(5),
                fail_open,
            },
        )
        .expect("client")
    }

    #[tokio::test]
    async fn checks_new_commits_and_applies_the_verdict() {
        let store = Arc::new(MemStore::new());
        store.create_repo("r").await.expect("repo");
        let a = commit(&store, vec![], "a.txt").await;
        let b = commit(&store, vec![a], "secret.env").await;
        let (url, seen) = hook(vec![
            (204, ""),
            (200, r#"{"allowed": false, "message": "contains a secret"}"#),
        ])
        .await;
        let h = pre(&store, &url, false);
        let alice = Principal::user("alice", vec!["dev".into()]);
        h.check(&alice, "r", "main", a).await.expect("allowed");
        store.cas_ref("r", "main", RefExpect::Absent, a).await.expect("land a");
        let err = h.check(&alice, "r", "main", b).await.expect_err("denied");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("contains a secret"), "{err:?}");
        // 已经合入的提交：没有新内容，不调用检查服务（服务只准备了两次回复）
        h.check(&alice, "r", "main", a).await.expect("nothing new");
        // 新分支指向默认分支上已有的提交：同样没有新内容
        h.check(&alice, "r", "feature", a).await.expect("existing content");

        let seen = seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 2);
        let (head, first) = &seen[0];
        assert!(head.contains("x-scion-event: pre-land") && head.contains("x-scion-signature-256: sha256="));
        assert_eq!(first["type"], "pre-land");
        assert!(first["old"].is_null());
        assert_eq!(first["pusher"]["subject"], "alice");
        assert_eq!(first["pusher"]["groups"][0], "dev");
        let second = &seen[1].1;
        assert_eq!(second["old"], a.to_string());
        assert_eq!(second["new"], b.to_string());
        assert_eq!(
            second["commits"].as_array().map(Vec::len),
            Some(1),
            "only commits not on the ref"
        );
        assert_eq!(second["commits"][0]["changes"][0]["path"], "secret.env");
    }

    #[tokio::test]
    async fn truncated_requests_need_explicit_acceptance() {
        let store = Arc::new(MemStore::new());
        store.create_repo("r").await.expect("repo");
        let mut tip = commit(&store, vec![], "f0").await;
        for i in 1..=MAX_COMMITS {
            tip = commit(&store, vec![tip], &format!("f{i}")).await;
        }
        let (url, seen) = hook(vec![(204, ""), (200, r#"{"allowed": true, "accept_truncated": true}"#)]).await;
        let h = pre(&store, &url, false);
        let p = Principal::anonymous();
        let err = h.check(&p, "r", "main", tip).await.expect_err("truncated");
        assert!(err.message().contains("smaller pieces"), "{err:?}");
        h.check(&p, "r", "main", tip).await.expect("accepted");
        let seen = seen.lock().expect("lock");
        assert_eq!(seen[0].1["commits_truncated"], true);
        assert_eq!(seen[0].1["commits"].as_array().map(Vec::len), Some(MAX_COMMITS));
    }

    #[tokio::test]
    async fn unavailable_checks_follow_the_failure_policy() {
        let store = Arc::new(MemStore::new());
        store.create_repo("r").await.expect("repo");
        let a = commit(&store, vec![], "a.txt").await;
        let p = Principal::anonymous();
        let (url, _) = hook(vec![(503, ""), (503, "")]).await;
        let closed = pre(&store, &url, false);
        let err = closed.check(&p, "r", "main", a).await.expect_err("closed");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        pre(&store, &url, true).check(&p, "r", "main", a).await.expect("open");
        // 无法创建客户端：一律拒绝，即使配置了放行
        let refuse = PreLand::refuse_all(
            store.clone(),
            store.clone(),
            PreLandConfig {
                url: url.clone(),
                secret: None,
                timeout: Duration::from_secs(1),
                fail_open: true,
            },
        );
        assert_eq!(
            refuse.check(&p, "r", "main", a).await.expect_err("refused").code(),
            tonic::Code::Unavailable
        );
        // 历史读不出来：同样按检查服务不可用处理
        let ghost = ObjectId::hash(scion_object::ObjectKind::Changeset, b"ghost");
        assert_eq!(
            closed.check(&p, "r", "main", ghost).await.expect_err("ghost").code(),
            tonic::Code::Unavailable
        );
    }

    #[test]
    fn verdicts_from_status_and_body() {
        assert_eq!(verdict(204, b"", false), Ok(Verdict::Allow));
        assert_eq!(verdict(200, br#"{"allowed": true}"#, false), Ok(Verdict::Allow));
        assert_eq!(
            verdict(200, br#"{"allowed": false, "message": "contains a secret"}"#, false),
            Ok(Verdict::Deny("contains a secret".into()))
        );
        assert_eq!(
            verdict(200, br#"{"allowed": false}"#, false),
            Ok(Verdict::Deny("rejected by the pre-land check".into()))
        );
        assert_eq!(
            verdict(403, b"  no binaries  ", false),
            Ok(Verdict::Deny("no binaries".into()))
        );
        assert_eq!(
            verdict(422, b"", false),
            Ok(Verdict::Deny("rejected by the pre-land check".into()))
        );
        assert!(verdict(408, b"", false).is_err());
        assert!(verdict(429, b"", false).is_err());
        assert!(verdict(500, b"", false).is_err());
        assert!(verdict(302, b"", false).is_err());
        // 截断的请求：2xx 不等于整次改动都通过了
        assert_eq!(verdict(204, b"", true), Ok(Verdict::Deny(TRUNCATED_MESSAGE.into())));
        assert_eq!(
            verdict(200, br#"{"allowed": true}"#, true),
            Ok(Verdict::Deny(TRUNCATED_MESSAGE.into()))
        );
        assert_eq!(
            verdict(200, br#"{"allowed": true, "accept_truncated": true}"#, true),
            Ok(Verdict::Allow)
        );
        assert_eq!(
            verdict(
                200,
                br#"{"allowed": false, "message": "no", "accept_truncated": true}"#,
                true
            ),
            Ok(Verdict::Deny("no".into()))
        );
        let long = "é".repeat(MAX_MESSAGE);
        let Ok(Verdict::Deny(m)) = verdict(403, long.as_bytes(), false) else {
            panic!("deny");
        };
        assert!(m.len() <= MAX_MESSAGE && m.is_char_boundary(m.len()));
    }
}
