//! The git gateway: git's smart HTTP protocol on Scion repositories ([design](../../../docs/design/git-gateway.md)).
//!
//! [`Gateway::serve`] answers `GET /<repo>/info/refs?service=git-upload-pack` and
//! `POST /<repo>/git-upload-pack`, with protocol v0 and v2. Pushing comes in stage G2.

pub mod objects;
pub mod pack;
pub mod pkt;
pub mod upload;

use std::convert::Infallible;
use std::sync::Arc;

use base64::Engine;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, Method, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use scion_auth::{Action, AuthError, Guard, Principal, Request as AuthRequest};
use scion_object::ObjectId;
use scion_store::{BlobStore, MetaStore};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::upload::{Advertised, FetchRequest, Out};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Query(#[from] scion_query::QueryError),
    #[error(transparent)]
    Store(#[from] scion_store::Error),
    /// The repository holds something git cannot be given (design §7).
    #[error("{0}")]
    Unservable(String),
    /// A feature of the protocol this gateway does not offer yet.
    #[error("{0}")]
    Unsupported(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("{0}")]
    Internal(String),
}

impl From<pkt::PktError> for Error {
    fn from(e: pkt::PktError) -> Self {
        Error::Protocol(e.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The response body: whole for advertisements and errors, streamed for packs.
pub type Body = BoxBody<Bytes, Infallible>;

/// The largest request body accepted: wants and haves, after gzip.
const MAX_REQUEST: usize = 16 << 20;

/// The endpoints of the smart HTTP protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    InfoRefs,
    UploadPack,
    ReceivePack,
}

/// The repository and endpoint of a git request, or `None` for any other request (gRPC among
/// them, whose paths are `/package.Service/Method`). A `.git` suffix on the repository is dropped.
pub fn route(method: &Method, path: &str, headers: &HeaderMap) -> Option<(String, Endpoint)> {
    let grpc = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"));
    if grpc {
        return None;
    }
    let (repo, endpoint) = if *method == Method::GET {
        (path.strip_suffix("/info/refs")?, Endpoint::InfoRefs)
    } else if *method == Method::POST {
        if let Some(r) = path.strip_suffix("/git-upload-pack") {
            (r, Endpoint::UploadPack)
        } else {
            (path.strip_suffix("/git-receive-pack")?, Endpoint::ReceivePack)
        }
    } else {
        return None;
    };
    let repo = repo.strip_prefix('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!repo.is_empty()).then(|| (repo.to_string(), endpoint))
}

/// Serves git's smart HTTP protocol from Scion's storage.
pub struct Gateway {
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
    guard: Arc<Guard>,
    agent: String,
}

impl Gateway {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>, guard: Arc<Guard>, version: &str) -> Self {
        Self {
            meta,
            blobs,
            guard,
            agent: format!("scion/{version}"),
        }
    }

    /// Answers one git request (one that [`route`] recognized).
    pub async fn serve<B>(&self, req: http::Request<B>, repo: String, endpoint: Endpoint) -> Response<Body>
    where
        B: http_body::Body<Data = Bytes> + Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (parts, body) = req.into_parts();
        let service = match endpoint {
            Endpoint::InfoRefs => parts
                .uri
                .query()
                .unwrap_or_default()
                .split('&')
                .find_map(|kv| kv.strip_prefix("service=")),
            Endpoint::UploadPack => Some("git-upload-pack"),
            Endpoint::ReceivePack => Some("git-receive-pack"),
        };
        match service {
            Some("git-upload-pack") => {}
            Some("git-receive-pack") => {
                return text(
                    StatusCode::FORBIDDEN,
                    "pushing with git is not supported yet; push with scn",
                );
            }
            _ => return text(StatusCode::FORBIDDEN, "only git's smart HTTP protocol is supported"),
        }
        let principal = match self.check(&parts.headers, &repo).await {
            Ok(p) => p,
            Err(r) => return *r,
        };
        let default = match self.meta.get_repo(&repo).await {
            Ok(Some(info)) if info.deleted_at_ms.is_none() => info.default_branch,
            Ok(_) => return text(StatusCode::NOT_FOUND, &format!("repository {repo} not found")),
            Err(e) => return failed(&e.into()),
        };
        let v2 = parts
            .headers
            .get("git-protocol")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(':').any(|p| p == "version=2"));
        tracing::debug!(%repo, subject = %principal.subject, ?endpoint, v2, "git request");
        if endpoint == Endpoint::InfoRefs {
            return self.info_refs(&repo, &default, v2).await;
        }
        let body = match read_body(&parts.headers, body).await {
            Ok(b) => b,
            Err(r) => return *r,
        };
        self.upload_pack(repo, default, v2, body).await
    }

    /// Authenticates the request and checks it may read the repository. Missing or bad
    /// credentials get a 401 that makes git ask for them.
    async fn check(&self, headers: &HeaderMap, repo: &str) -> std::result::Result<Principal, Box<Response<Body>>> {
        let token = credentials(headers);
        let p = match self.guard.authenticate(token.as_deref()).await {
            Ok(p) => p,
            Err(e) => return Err(Box::new(auth_error(e, true))),
        };
        match self.guard.authorize(&p, &AuthRequest::repo(repo, Action::Read)).await {
            Ok(()) => Ok(p),
            Err(e) => Err(Box::new(auth_error(e, token.is_none()))),
        }
    }

    async fn advertised(&self, repo: &str, default: &str) -> Result<Advertised> {
        upload::advertised(self.meta.as_ref(), self.blobs.as_ref(), repo, default).await
    }

    async fn info_refs(&self, repo: &str, default: &str, v2: bool) -> Response<Body> {
        let mut out = BytesMut::new();
        if v2 {
            // As git http-backend: no service line before a v2 advertisement
            out = upload::advertise_v2(&self.agent);
        } else {
            let adv = match self.advertised(repo, default).await {
                Ok(a) => a,
                Err(e) => return failed(&e),
            };
            pkt::put_line(&mut out, "# service=git-upload-pack");
            out.extend_from_slice(pkt::FLUSH);
            out.extend_from_slice(&upload::advertise_v0(&adv, &self.agent));
        }
        git_response("application/x-git-upload-pack-advertisement", full(out.freeze()))
    }

    async fn upload_pack(&self, repo: String, default: String, v2: bool, body: Bytes) -> Response<Body> {
        const RESULT: &str = "application/x-git-upload-pack-result";
        if v2 {
            let req = match upload::parse_v2(&body) {
                Ok(r) => r,
                Err(e) => return git_response(RESULT, full(err_pkt(&e))),
            };
            match req.command.as_str() {
                "ls-refs" => match self.advertised(&repo, &default).await {
                    Ok(adv) => git_response(RESULT, full(upload::ls_refs(&adv, &req.args).freeze())),
                    Err(e) => git_response(RESULT, full(err_pkt(&e))),
                },
                "fetch" => match upload::parse_fetch_v2(&req.args) {
                    Ok(f) => git_response(RESULT, self.fetch(repo, f, true)),
                    Err(e) => git_response(RESULT, full(err_pkt(&e))),
                },
                other => git_response(
                    RESULT,
                    full(err_pkt(&Error::Unsupported(format!("unknown command {other:?}")))),
                ),
            }
        } else {
            match upload::parse_fetch_v0(&body) {
                Ok(f) => git_response(RESULT, self.fetch(repo, f, false)),
                Err(e) => git_response(RESULT, full(err_pkt(&e))),
            }
        }
    }

    /// Negotiates and, once ready, streams the pack.
    fn fetch(&self, repo: String, req: FetchRequest, v2: bool) -> Body {
        let (tx, rx) = mpsc::channel::<Bytes>(16);
        let meta = self.meta.clone();
        let blobs = self.blobs.clone();
        tokio::spawn(async move {
            fetch(meta.as_ref(), blobs.as_ref(), &repo, &req, v2, &tx).await;
        });
        let stream = ReceiverStream::new(rx).map(|b| Ok::<_, Infallible>(http_body::Frame::data(b)));
        BoxBody::new(StreamBody::new(stream))
    }
}

/// Answers a fetch request into `tx`. An error before the pack starts goes to the client as an
/// `ERR` packet; once the pack has started, on side band 3 (without side bands the pack just ends).
async fn fetch(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    repo: &str,
    req: &FetchRequest,
    v2: bool,
    tx: &mpsc::Sender<Bytes>,
) {
    if req.wants.is_empty() {
        return;
    }
    let (wants, haves) = match negotiate(meta, repo, req, v2).await {
        Ok((head, found)) => {
            if tx.send(head.freeze()).await.is_err() {
                return;
            }
            match found {
                Some(f) => f,
                None => return,
            }
        }
        Err(e) => {
            tracing::warn!(%repo, error = %e, "git fetch failed");
            let _ = tx.send(err_pkt(&e)).await;
            return;
        }
    };
    let out = Out::new(tx.clone(), req.side_band, !req.no_progress);
    match send_pack(meta, blobs, repo, &wants, &haves, &out).await {
        Ok(()) => out.flush().await,
        Err(e) => {
            tracing::warn!(%repo, error = %e, "git fetch failed");
            out.error(&e.to_string()).await;
        }
    }
}

/// Wanted and common commits, once ready to send the pack (`None` when the client should send
/// more haves).
type Found = Option<(Vec<ObjectId>, Vec<ObjectId>)>;

/// The acknowledgments, and what the pack is to hold. Wants this repository does not have are
/// refused before anything else is sent.
async fn negotiate(meta: &dyn MetaStore, repo: &str, req: &FetchRequest, v2: bool) -> Result<(BytesMut, Found)> {
    let wants = upload::resolve_wants(meta, repo, &req.wants).await?;
    let common = upload::common(meta, repo, &req.haves).await?;
    let ready = req.done || upload::ready(req, common.len());
    let ids = common.iter().map(|(_, id)| *id).collect();
    let mut head = BytesMut::new();
    if v2 {
        if !req.done {
            pkt::put_line(&mut head, "acknowledgments");
            if common.is_empty() {
                pkt::put_line(&mut head, "NAK");
            }
            for (sha, _) in &common {
                pkt::put_line(&mut head, &format!("ACK {}", hex::encode(sha)));
            }
            if !ready {
                head.extend_from_slice(pkt::FLUSH);
                return Ok((head, None));
            }
            pkt::put_line(&mut head, "ready");
            head.extend_from_slice(pkt::DELIM);
        }
        pkt::put_line(&mut head, "packfile");
        return Ok((head, Some((wants, ids))));
    }
    let last = common.last().map(|(s, _)| hex::encode(s));
    if req.multi_ack {
        for (sha, _) in &common {
            pkt::put_line(&mut head, &format!("ACK {} common", hex::encode(sha)));
        }
        if let Some(l) = last.as_ref().filter(|_| ready) {
            pkt::put_line(&mut head, &format!("ACK {l} ready"));
        }
    }
    if !(req.done || (req.no_done && ready && req.multi_ack)) {
        pkt::put_line(&mut head, "NAK");
        return Ok((head, None));
    }
    match &last {
        Some(l) => pkt::put_line(&mut head, &format!("ACK {l}")),
        None => pkt::put_line(&mut head, "NAK"),
    }
    Ok((head, Some((wants, ids))))
}

async fn send_pack(
    meta: &dyn MetaStore,
    blobs: &dyn BlobStore,
    repo: &str,
    wants: &[ObjectId],
    haves: &[ObjectId],
    out: &Out,
) -> Result<()> {
    let plan = upload::plan(meta, blobs, repo, wants, haves).await?;
    upload::check_redactions(meta, repo, &plan.files).await?;
    upload::write_pack(meta, blobs, repo, &plan, out).await
}

/// The token in `Authorization`: the password of Basic credentials, or a Bearer token.
fn credentials(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.trim().to_string());
    }
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
    let pair = String::from_utf8(raw).ok()?;
    let (_, password) = pair.split_once(':')?;
    (!password.is_empty()).then(|| password.to_string())
}

/// 401 asks git for credentials; 403 when the credentials it sent cannot read the repository.
fn auth_error(e: AuthError, anonymous: bool) -> Response<Body> {
    match e {
        AuthError::Missing | AuthError::Expired | AuthError::Invalid(_) => unauthorized(&e.to_string()),
        AuthError::Denied { .. } if anonymous => unauthorized("authentication required"),
        AuthError::Denied { .. } => text(StatusCode::FORBIDDEN, &e.to_string()),
        AuthError::Unavailable(_) => text(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
        AuthError::BadRequest(_) => text(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

fn unauthorized(msg: &str) -> Response<Body> {
    let mut r = text(StatusCode::UNAUTHORIZED, msg);
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        http::HeaderValue::from_static("Basic realm=\"Scion\""),
    );
    r
}

/// The request body, gunzipped when git compressed it.
async fn read_body<B>(headers: &HeaderMap, body: B) -> std::result::Result<Bytes, Box<Response<Body>>>
where
    B: http_body::Body<Data = Bytes> + Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let raw = match Limited::new(body, MAX_REQUEST).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return Err(Box::new(text(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("request body: {e}"),
            )));
        }
    };
    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    if !gzip {
        return Ok(raw);
    }
    use std::io::Read;
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(&raw[..]).take(MAX_REQUEST as u64 + 1);
    if let Err(e) = dec.read_to_end(&mut out) {
        return Err(Box::new(text(
            StatusCode::BAD_REQUEST,
            &format!("gzip request body: {e}"),
        )));
    }
    if out.len() > MAX_REQUEST {
        return Err(Box::new(text(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")));
    }
    Ok(Bytes::from(out))
}

fn full(b: Bytes) -> Body {
    BoxBody::new(Full::new(b))
}

fn err_pkt(e: &Error) -> Bytes {
    let mut out = BytesMut::new();
    pkt::put_line(&mut out, &format!("ERR {e}"));
    out.freeze()
}

fn git_response(content_type: &'static str, body: Body) -> Response<Body> {
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, http::HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    r
}

/// A plain-text error, which git prints as `remote: …`.
fn text(status: StatusCode, msg: &str) -> Response<Body> {
    let mut r = Response::new(full(Bytes::from(format!("{msg}\n"))));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

fn failed(e: &Error) -> Response<Body> {
    tracing::warn!(error = %e, "git request failed");
    text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().expect("value"));
        }
        h
    }

    #[test]
    fn routes_git_requests_only() {
        let none = HeaderMap::new();
        assert_eq!(
            route(&Method::GET, "/agentlab/app/info/refs", &none),
            Some(("agentlab/app".into(), Endpoint::InfoRefs))
        );
        assert_eq!(
            route(&Method::POST, "/agentlab/app.git/git-upload-pack", &none),
            Some(("agentlab/app".into(), Endpoint::UploadPack))
        );
        assert_eq!(
            route(&Method::POST, "/mono/git-receive-pack", &none),
            Some(("mono".into(), Endpoint::ReceivePack))
        );
        let grpc = headers(&[("content-type", "application/grpc")]);
        assert_eq!(route(&Method::POST, "/x/git-upload-pack", &grpc), None);
        assert_eq!(route(&Method::POST, "/scion.v1.QueryService/Log", &none), None);
        assert_eq!(route(&Method::GET, "/info/refs", &none), None);
        assert_eq!(route(&Method::PUT, "/x/info/refs", &none), None);
        assert_eq!(route(&Method::GET, "/x/git-upload-pack", &none), None);
    }

    #[test]
    fn reads_credentials() {
        // "git:scn_pat_x" and ":scn_pat_x"
        let basic = headers(&[("authorization", "Basic Z2l0OnNjbl9wYXRfeA==")]);
        assert_eq!(credentials(&basic).as_deref(), Some("scn_pat_x"));
        let no_user = headers(&[("authorization", "basic OnNjbl9wYXRfeA==")]);
        assert_eq!(credentials(&no_user).as_deref(), Some("scn_pat_x"));
        let bearer = headers(&[("authorization", "Bearer abc")]);
        assert_eq!(credentials(&bearer).as_deref(), Some("abc"));
        // "git:" has no password; garbage and other schemes are no credentials
        for bad in ["Basic Z2l0Og==", "Basic !!!", "Digest x", "Basic"] {
            assert_eq!(credentials(&headers(&[("authorization", bad)])), None, "{bad}");
        }
    }

    #[test]
    fn auth_errors_ask_for_credentials() {
        let denied = || AuthError::Denied {
            repo: "r".into(),
            action: "read",
            reference: None,
            path: None,
        };
        let r = auth_error(denied(), true);
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(r.headers().contains_key(header::WWW_AUTHENTICATE));
        assert_eq!(auth_error(denied(), false).status(), StatusCode::FORBIDDEN);
        assert_eq!(auth_error(AuthError::Expired, false).status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            auth_error(AuthError::Unavailable("x".into()), false).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            auth_error(AuthError::BadRequest("x".into()), false).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn reads_gzipped_bodies() {
        use std::io::Write;
        let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(b"0009done\n").expect("gzip");
        let gz = z.finish().expect("gzip");
        let h = headers(&[("content-encoding", "gzip")]);
        let body = read_body(&h, Full::new(Bytes::from(gz))).await.expect("body");
        assert_eq!(&body[..], b"0009done\n");
        let plain = read_body(&HeaderMap::new(), Full::new(Bytes::from_static(b"x"))).await;
        assert_eq!(plain.expect("body"), Bytes::from_static(b"x"));
        let bad = read_body(&h, Full::new(Bytes::from_static(b"not gzip"))).await;
        assert_eq!(bad.expect_err("bad gzip").status(), StatusCode::BAD_REQUEST);
    }

    async fn gateway() -> Gateway {
        let s = Arc::new(scion_store::MemStore::new());
        s.create_repo("r").await.expect("repo");
        Gateway::new(s.clone(), s, Guard::disabled(), "0.0.0")
    }

    async fn call(g: &Gateway, method: Method, uri: &str, v2: bool, body: &'static [u8]) -> (StatusCode, Bytes) {
        let mut b = http::Request::builder().method(method).uri(uri);
        if v2 {
            b = b.header("git-protocol", "version=2");
        }
        let req = b.body(Full::new(Bytes::from_static(body))).expect("request");
        let (repo, endpoint) = route(req.method(), req.uri().path(), req.headers()).expect("git request");
        let resp = g.serve(req, repo, endpoint).await;
        let status = resp.status();
        (status, resp.into_body().collect().await.expect("body").to_bytes())
    }

    #[tokio::test]
    async fn answers_requests_it_cannot_serve() {
        let g = gateway().await;
        let (st, body) = call(&g, Method::GET, "/r/info/refs", false, b"").await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&body).contains("smart HTTP"));
        let (st, _) = call(&g, Method::GET, "/r/info/refs?service=git-receive-pack", false, b"").await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = call(
            &g,
            Method::GET,
            "/missing/info/refs?service=git-upload-pack",
            false,
            b"",
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // An empty repository advertises no refs, with v0 and v2
        let (st, body) = call(&g, Method::GET, "/r/info/refs?service=git-upload-pack", false, b"").await;
        assert_eq!(st, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("capabilities^{}"));
        let (_, body) = call(&g, Method::GET, "/r/info/refs?service=git-upload-pack", true, b"").await;
        assert!(body.starts_with(b"000eversion 2\n"), "{body:?}");
    }

    #[tokio::test]
    async fn reports_protocol_errors_as_err_packets() {
        let g = gateway().await;
        let cases: [(bool, &'static [u8], &str); 5] = [
            (true, b"0011command=nope\n0000", "unknown command"),
            (true, b"zzzz", "bad length"),
            (true, b"0011command=fetch0001000ddeepen 1\n0000", "shallow"),
            (false, b"000ddeepen 1\n0000", "shallow"),
            (true, b"0014command=ls-refs\n0000", ""),
        ];
        for (v2, body, want) in cases {
            let (st, got) = call(&g, Method::POST, "/r/git-upload-pack", v2, body).await;
            assert_eq!(st, StatusCode::OK);
            let got = String::from_utf8_lossy(&got).into_owned();
            if want.is_empty() {
                assert_eq!(got, "0000");
            } else {
                assert!(got.contains("ERR ") && got.contains(want), "{want}: {got}");
            }
        }
        // No wants: nothing to send
        let (_, got) = call(&g, Method::POST, "/r/git-upload-pack", false, b"0000").await;
        assert!(got.is_empty());
        // A want this repository does not have
        let (_, got) = call(
            &g,
            Method::POST,
            "/r/git-upload-pack",
            false,
            b"0032want 0101010101010101010101010101010101010101\n00000009done\n",
        )
        .await;
        assert!(String::from_utf8_lossy(&got).contains("not our ref"), "{got:?}");
    }
}
