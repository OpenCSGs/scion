//! deadline（服务端设计文档 §5.3）：取客户端的 `grpc-timeout` 与服务端上限中较小的一个。
//!
//! 超时返回 DEADLINE_EXCEEDED。deadline 放进请求的 extensions（[`Deadline`]），向上游转发时用
//! [`Deadline::remaining`] 设置剩余时间，整条调用链共享同一个截止时间。
//!
//! tonic 自身也按 `grpc-timeout` 计时（超时返回 CANCELLED，且先于本层触发）；本层主要负责服务端上限、
//! 指标和向下游传递。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tonic::Status;
use tower::{Layer, Service};

use crate::metrics::{metrics, reason};

/// 本次请求的截止时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(pub Instant);

impl Deadline {
    /// 剩余时间（已过期时为 0）。
    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// 请求 extensions 中的 deadline。
    pub fn of(ext: &http::Extensions) -> Option<Self> {
        ext.get::<Self>().copied()
    }
}

/// 解析 `grpc-timeout`：最多 8 位数字加单位（H 时、M 分、S 秒、m 毫秒、u 微秒、n 纳秒）。
pub fn parse_grpc_timeout(v: &str) -> Option<Duration> {
    if v.len() < 2 || v.len() > 9 {
        return None;
    }
    let (digits, unit) = v.split_at(v.len() - 1);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n * 3600),
        "M" => Duration::from_secs(n * 60),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// `max` 为服务端上限；`None` 时只按客户端的 deadline。
#[derive(Debug, Clone, Copy, Default)]
pub struct DeadlineLayer {
    max: Option<Duration>,
}

impl DeadlineLayer {
    pub fn new(max: Option<Duration>) -> Self {
        Self {
            max: max.filter(|d| !d.is_zero()),
        }
    }
}

impl<S> Layer<S> for DeadlineLayer {
    type Service = DeadlineService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        DeadlineService { inner, max: self.max }
    }
}

#[derive(Debug, Clone)]
pub struct DeadlineService<S> {
    inner: S,
    max: Option<Duration>,
}

type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for DeadlineService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    S::Future: Send + 'static,
    ResBody: Default + Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<ReqBody>) -> Self::Future {
        let client = req
            .headers()
            .get("grpc-timeout")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_grpc_timeout);
        let limit = match (client, self.max) {
            (Some(c), Some(m)) => Some(c.min(m)),
            (c, m) => c.or(m),
        };
        let Some(limit) = limit else {
            return Box::pin(self.inner.call(req));
        };
        req.extensions_mut().insert(Deadline(Instant::now() + limit));
        let fut = self.inner.call(req);
        Box::pin(async move {
            match tokio::time::timeout(limit, fut).await {
                Ok(r) => r,
                Err(_) => {
                    metrics().reject(reason::DEADLINE);
                    Ok(Status::deadline_exceeded(format!("deadline of {limit:?} exceeded")).into_http())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[test]
    fn parses_grpc_timeouts() {
        assert_eq!(parse_grpc_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_grpc_timeout("2M"), Some(Duration::from_secs(120)));
        assert_eq!(parse_grpc_timeout("3S"), Some(Duration::from_secs(3)));
        assert_eq!(parse_grpc_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_grpc_timeout("7u"), Some(Duration::from_micros(7)));
        assert_eq!(parse_grpc_timeout("99999999n"), Some(Duration::from_nanos(99_999_999)));
        for bad in ["", "S", "1x", "123456789S", "-1S", "1.5S"] {
            assert_eq!(parse_grpc_timeout(bad), None, "{bad}");
        }
        let d = Deadline(Instant::now() + Duration::from_secs(60));
        assert!(d.remaining() > Duration::from_secs(59));
        assert_eq!(
            Deadline(Instant::now() - Duration::from_secs(1)).remaining(),
            Duration::ZERO
        );
    }

    /// 测试用服务：等待请求中给出的时长，返回看到的 deadline。
    #[derive(Clone)]
    struct Sleepy;

    impl Service<http::Request<Duration>> for Sleepy {
        type Response = http::Response<String>;
        type Error = Infallible;
        type Future = BoxFuture<Self::Response, Infallible>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<Duration>) -> Self::Future {
            let seen = Deadline::of(req.extensions()).map(|d| d.remaining());
            let wait = *req.body();
            Box::pin(async move {
                tokio::time::sleep(wait).await;
                Ok(http::Response::new(format!("{}", seen.is_some())))
            })
        }
    }

    fn request(timeout: Option<&str>, wait: Duration) -> http::Request<Duration> {
        let mut b = http::Request::builder().uri("/s.S/M");
        if let Some(t) = timeout {
            b = b.header("grpc-timeout", t);
        }
        b.body(wait).expect("request")
    }

    fn code(resp: &http::Response<String>) -> Option<tonic::Code> {
        Status::from_header_map(resp.headers()).map(|s| s.code())
    }

    #[tokio::test(start_paused = true)]
    async fn enforces_the_smaller_deadline() {
        let short = Duration::from_millis(10);
        // 只有服务端上限
        let mut svc = DeadlineLayer::new(Some(Duration::from_secs(1))).layer(Sleepy);
        let r = svc.call(request(None, Duration::from_secs(2))).await.expect("resp");
        assert_eq!(code(&r), Some(tonic::Code::DeadlineExceeded));
        let r = svc.call(request(None, short)).await.expect("resp");
        assert_eq!(r.into_body(), "true", "deadline visible to the handler");
        // 客户端更短
        let r = svc.call(request(Some("5m"), short)).await.expect("resp");
        assert_eq!(code(&r), Some(tonic::Code::DeadlineExceeded));
        // 没有任何 deadline：不限制
        let mut free = DeadlineLayer::new(Some(Duration::ZERO)).layer(Sleepy);
        let r = free
            .call(request(Some("bogus"), Duration::from_secs(5)))
            .await
            .expect("resp");
        assert_eq!(r.into_body(), "false");
        // 只有客户端 deadline
        let r = free.call(request(Some("1S"), short)).await.expect("resp");
        assert_eq!(r.into_body(), "true");
        std::future::poll_fn(|cx| free.poll_ready(cx)).await.expect("ready");
    }
}
