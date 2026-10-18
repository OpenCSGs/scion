//! gRPC 服务端中间件：按方法记录请求数、状态码与延迟（[`MetricsLayer`]）。
//!
//! 状态码可能在响应头里（只有 trailers 的错误响应），也可能在响应体结束时的 trailers 里，所以包装响应体，
//! 在流结束时记录。客户端中途断开（响应体未读完就被丢弃）记为 `CANCELLED`。
//!
//! 本层在认证之外，路径由调用方任意给出：只给已知的方法建时间序列，其它路径归入 `unknown`，
//! 否则换着路径请求就能让注册表无限增长。

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body::{Body, Frame, SizeHint};
use tower::{Layer, Service};

use crate::metrics::{MethodLabels, RpcLabels, metrics};

/// gRPC 状态码的规范名称（`grpc-status` 头的值）。
pub fn code_name(code: tonic::Code) -> &'static str {
    use tonic::Code::*;
    match code {
        Ok => "OK",
        Cancelled => "CANCELLED",
        Unknown => "UNKNOWN",
        InvalidArgument => "INVALID_ARGUMENT",
        DeadlineExceeded => "DEADLINE_EXCEEDED",
        NotFound => "NOT_FOUND",
        AlreadyExists => "ALREADY_EXISTS",
        PermissionDenied => "PERMISSION_DENIED",
        ResourceExhausted => "RESOURCE_EXHAUSTED",
        FailedPrecondition => "FAILED_PRECONDITION",
        Aborted => "ABORTED",
        OutOfRange => "OUT_OF_RANGE",
        Unimplemented => "UNIMPLEMENTED",
        Internal => "INTERNAL",
        Unavailable => "UNAVAILABLE",
        DataLoss => "DATA_LOSS",
        Unauthenticated => "UNAUTHENTICATED",
    }
}

/// 把 `/scion.v1.ObjectService/GetObjects` 拆成 (`scion.v1.ObjectService`, `GetObjects`)。
pub fn split_path(path: &str) -> (&str, &str) {
    let p = path.strip_prefix('/').unwrap_or(path);
    p.split_once('/').unwrap_or((p, ""))
}

fn status_of(headers: &http::HeaderMap) -> Option<tonic::Code> {
    let v = headers.get("grpc-status")?.to_str().ok()?.parse::<i32>().ok()?;
    Some(tonic::Code::from_i32(v))
}

/// 不在已知方法中的请求使用的标签值。
pub const UNKNOWN: &str = "unknown";

/// gRPC 标准健康检查的方法（`grpc.health.v1`）。
pub const HEALTH_METHODS: &[&str] = &["/grpc.health.v1.Health/Check", "/grpc.health.v1.Health/Watch"];

#[derive(Debug, Clone)]
pub struct MetricsLayer {
    known: Arc<HashSet<String>>,
}

impl MetricsLayer {
    /// `known` 为已知方法的路径（如 `/scion.v1.ObjectService/GetObjects`）。
    pub fn new<'a>(known: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            known: Arc::new(known.into_iter().map(str::to_owned).collect()),
        }
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService {
            inner,
            known: self.known.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricsService<S> {
    inner: S,
    known: Arc<HashSet<String>>,
}

/// 一个进行中的请求；结束（或被丢弃）时记录一次。
struct Pending {
    service: String,
    method: String,
    started: Instant,
    code: Option<tonic::Code>,
}

impl Pending {
    fn new(path: &str, known: &HashSet<String>) -> Self {
        let (service, method) = if known.contains(path) {
            split_path(path)
        } else {
            (UNKNOWN, UNKNOWN)
        };
        metrics().rpc_in_flight.inc();
        Self {
            service: service.to_owned(),
            method: method.to_owned(),
            started: Instant::now(),
            code: None,
        }
    }

    fn finish(self, code: tonic::Code) {
        let m = metrics();
        m.rpc_in_flight.dec();
        m.rpc_requests
            .get_or_create(&RpcLabels {
                service: self.service.clone(),
                method: self.method.clone(),
                code: code_name(code).to_owned(),
            })
            .inc();
        m.rpc_duration
            .get_or_create(&MethodLabels {
                service: self.service,
                method: self.method,
            })
            .observe(self.started.elapsed().as_secs_f64());
    }
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for MetricsService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
{
    type Response = http::Response<MeteredBody<ResBody>>;
    type Error = S::Error;
    type Future = MetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let pending = Pending::new(req.uri().path(), &self.known);
        MetricsFuture {
            inner: self.inner.call(req),
            pending: Some(pending),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct MetricsFuture<F> {
        #[pin]
        inner: F,
        pending: Option<Pending>,
    }

    impl<F> PinnedDrop for MetricsFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            // 还没有响应就被丢弃：连接断开或请求被取消
            if let Some(p) = this.project().pending.take() {
                p.finish(tonic::Code::Cancelled);
            }
        }
    }
}

impl<F, B, E> std::future::Future for MetricsFuture<F>
where
    F: std::future::Future<Output = Result<http::Response<B>, E>>,
{
    type Output = Result<http::Response<MeteredBody<B>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let out = std::task::ready!(this.inner.poll(cx));
        let Some(mut pending) = this.pending.take() else {
            return Poll::Pending; // 不会发生：完成后不再被 poll
        };
        Poll::Ready(match out {
            Ok(resp) => {
                pending.code = status_of(resp.headers());
                Ok(resp.map(|body| MeteredBody {
                    inner: body,
                    pending: Some(pending),
                }))
            }
            Err(e) => {
                pending.finish(tonic::Code::Unknown);
                Err(e)
            }
        })
    }
}

pin_project_lite::pin_project! {
    /// 在结束时记录状态码与延迟的响应体。
    pub struct MeteredBody<B> {
        #[pin]
        inner: B,
        pending: Option<Pending>,
    }

    impl<B> PinnedDrop for MeteredBody<B> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(p) = this.project().pending.take() {
                let code = p.code.unwrap_or(tonic::Code::Cancelled);
                p.finish(code);
            }
        }
    }
}

impl<B: Body> Body for MeteredBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let frame = std::task::ready!(this.inner.poll_frame(cx));
        match &frame {
            Some(Ok(f)) => {
                if let (Some(t), Some(p)) = (f.trailers_ref(), this.pending.as_mut()) {
                    p.code = status_of(t).or(p.code);
                }
            }
            Some(Err(_)) => {
                if let Some(p) = this.pending.take() {
                    p.finish(tonic::Code::Internal);
                }
            }
            None => {
                if let Some(p) = this.pending.take() {
                    let code = p.code.unwrap_or(tonic::Code::Unknown);
                    p.finish(code);
                }
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    use http_body_util::BodyExt;

    fn count(service: &str, method: &str, code: &str) -> u64 {
        metrics()
            .rpc_requests
            .get_or_create(&RpcLabels {
                service: service.into(),
                method: method.into(),
                code: code.into(),
            })
            .get()
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(code_name(tonic::Code::Ok), "OK");
        assert_eq!(code_name(tonic::Code::ResourceExhausted), "RESOURCE_EXHAUSTED");
        let all = (0..=16)
            .map(|i| code_name(tonic::Code::from_i32(i)))
            .collect::<Vec<_>>();
        assert_eq!(all.len(), 17);
        assert!(
            all.iter()
                .all(|n| n.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
        );
        assert_eq!(split_path("/scion.v1.X/Get"), ("scion.v1.X", "Get"));
        assert_eq!(split_path("odd"), ("odd", ""));
        assert_eq!(HEALTH_METHODS.len(), 2);
    }

    /// 测试用服务：按路径返回不同的响应。
    #[derive(Clone)]
    struct Fake;

    impl Service<http::Request<()>> for Fake {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = &'static str;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<()>) -> Self::Future {
            let mut resp = http::Response::new(http_body_util::Full::new(bytes::Bytes::from_static(b"x")));
            match req.uri().path() {
                "/t.S/Fail" => return std::future::ready(Err("transport")),
                "/t.S/Header" => {
                    resp.headers_mut()
                        .insert("grpc-status", http::HeaderValue::from_static("5"));
                }
                _ => {}
            }
            std::future::ready(Ok(resp))
        }
    }

    fn request(path: &str) -> http::Request<()> {
        http::Request::builder().uri(path).body(()).expect("request")
    }

    #[tokio::test]
    async fn records_codes_from_headers_body_end_and_drops() {
        let paths = ["/t.S/Header", "/t.S/Plain", "/t.S/Dropped", "/t.S/Never", "/t.S/Fail"];
        let mut svc = MetricsLayer::new(paths).layer(Fake);
        // 响应头里的状态码（只有 trailers 的错误响应）
        let before = count("t.S", "Header", "NOT_FOUND");
        let resp = svc.call(request("/t.S/Header")).await.expect("resp");
        resp.into_body().collect().await.expect("body");
        assert_eq!(count("t.S", "Header", "NOT_FOUND"), before + 1);
        // 没有状态码的响应体读完：UNKNOWN
        let before = count("t.S", "Plain", "UNKNOWN");
        svc.call(request("/t.S/Plain"))
            .await
            .expect("resp")
            .into_body()
            .collect()
            .await
            .expect("body");
        assert_eq!(count("t.S", "Plain", "UNKNOWN"), before + 1);
        // 响应体没读完就被丢弃：CANCELLED
        let before = count("t.S", "Dropped", "CANCELLED");
        drop(svc.call(request("/t.S/Dropped")).await.expect("resp"));
        assert_eq!(count("t.S", "Dropped", "CANCELLED"), before + 1);
        // 还没有响应的请求被丢弃：CANCELLED
        let before = count("t.S", "Never", "CANCELLED");
        drop(svc.call(request("/t.S/Never")));
        assert_eq!(count("t.S", "Never", "CANCELLED"), before + 1);
        // 服务出错：UNKNOWN
        let before = count("t.S", "Fail", "UNKNOWN");
        assert!(svc.call(request("/t.S/Fail")).await.is_err());
        assert_eq!(count("t.S", "Fail", "UNKNOWN"), before + 1);
        // 未知路径归入固定的标签，不新建时间序列
        let before = count(UNKNOWN, UNKNOWN, "UNKNOWN");
        for i in 0..3 {
            let resp = svc.call(request(&format!("/attacker.S/M{i}"))).await.expect("resp");
            resp.into_body().collect().await.expect("body");
        }
        assert_eq!(count(UNKNOWN, UNKNOWN, "UNKNOWN"), before + 3);
        assert!(!metrics().encode().contains("attacker"));
        std::future::poll_fn(|cx| svc.poll_ready(cx)).await.expect("ready");
    }

    /// 测试用响应体：先数据，再 trailers（或错误）。
    struct Frames(Vec<Result<Frame<bytes::Bytes>, Infallible>>, bool);

    impl Body for Frames {
        type Data = bytes::Bytes;
        type Error = &'static str;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<bytes::Bytes>, &'static str>>> {
            if self.1 {
                self.1 = false;
                return Poll::Ready(Some(Err("broken")));
            }
            Poll::Ready(self.0.pop().map(|f| f.map_err(|_| "never")))
        }
    }

    #[tokio::test]
    async fn trailers_and_body_errors() {
        let known: HashSet<String> = ["/t.B/Trailers", "/t.B/Broken"].map(str::to_owned).into();
        let pending = |m: &str| {
            let mut p = Pending::new(&format!("/t.B/{m}"), &known);
            p.code = None;
            p
        };
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        let before = count("t.B", "Trailers", "OK");
        let body = MeteredBody {
            inner: Frames(
                vec![
                    Ok(Frame::trailers(trailers)),
                    Ok(Frame::data(bytes::Bytes::from_static(b"d"))),
                ],
                false,
            ),
            pending: Some(pending("Trailers")),
        };
        assert_eq!(body.size_hint().lower(), 0);
        assert!(!body.is_end_stream());
        body.collect().await.expect("collect");
        assert_eq!(count("t.B", "Trailers", "OK"), before + 1);

        let before = count("t.B", "Broken", "INTERNAL");
        let body = MeteredBody {
            inner: Frames(Vec::new(), true),
            pending: Some(pending("Broken")),
        };
        assert!(body.collect().await.is_err());
        assert_eq!(count("t.B", "Broken", "INTERNAL"), before + 1);
    }
}
