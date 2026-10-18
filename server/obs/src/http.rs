//! `/metrics` 端点（独立端口，不经 gRPC 的认证与限流）。

use std::future::Future;

use tokio::net::TcpListener;

use crate::metrics::metrics;

/// OpenMetrics 文本格式。
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// 在 `listener` 上提供 `GET /metrics`，直到 `shutdown` 完成。
pub async fn serve_metrics(
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(|| async { ([(::http::header::CONTENT_TYPE, CONTENT_TYPE)], metrics().encode()) }),
    );
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn serves_metrics_over_http() {
        metrics().reject(crate::reason::DEADLINE);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_metrics(listener, async {
            let _ = rx.await;
        }));
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        s.write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut out = String::new();
        s.read_to_string(&mut out).await.expect("read");
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("application/openmetrics-text"));
        assert!(out.contains("scion_rejected_requests_total{reason=\"deadline\"}"));
        let _ = tx.send(());
        server.await.expect("join").expect("serve");
    }
}
