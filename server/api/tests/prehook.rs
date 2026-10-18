//! 合入前检查端到端：检查服务拒绝含 secret 路径的改动，合入与建分支都受约束，回退不需要检查。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use scion_api::prehook::PreLandConfig;
use scion_api::{App, ServerConfig, serve};
use scion_client::Client;
use scion_proto::v1::ErrorReason;
use scion_store::{MemStore, MetaStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 检查服务：改动路径中出现 "secret" 时拒绝。返回地址和收到的请求数。
async fn checker() -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/check", listener.local_addr().expect("addr"));
    let calls: Arc<Mutex<usize>> = Arc::default();
    let n = calls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let n = n.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let r = s.read(&mut chunk).await.unwrap_or(0);
                    if r == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..r]);
                    let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    if buf.len() < i + 4 + len {
                        continue;
                    }
                    let body: serde_json::Value = serde_json::from_slice(&buf[i + 4..i + 4 + len]).expect("json");
                    buf.drain(..i + 4 + len);
                    *n.lock().expect("lock") += 1;
                    let secret = body["commits"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|c| c["changes"].as_array().cloned().unwrap_or_default())
                        .any(|ch| ch["path"].as_str().is_some_and(|p| p.contains("secret")));
                    let resp: &[u8] = if secret {
                        b"HTTP/1.1 403 Forbidden\r\ncontent-length: 19\r\n\r\nsecrets not allowed"
                    } else {
                        b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n"
                    };
                    let _ = s.write_all(resp).await;
                }
            });
        }
    });
    (url, calls)
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_land_check_gates_lands_and_ref_writes() {
    let (hook, calls) = checker().await;
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let app = App {
        config: Arc::new(ServerConfig {
            warm_record_cache: false,
            pre_land: Some(PreLandConfig {
                url: hook,
                secret: None,
                timeout: Duration::from_secs(5),
                fail_open: false,
            }),
            ..ServerConfig::default()
        }),
        meta: store.clone(),
        blobs: store.clone(),
    };
    let (_stop, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, app, async {
        let _ = rx.await;
    }));
    let c = Client::builder()
        .endpoint(&url)
        .repo("mono")
        .build()
        .await
        .expect("connect");

    let ok = c.changeset(None).put("README.md", "hi").create().await.expect("commit");
    c.land("main", &ok.id).await.expect("allowed");
    let bad = c
        .changeset(Some(ok.id))
        .put("config/secret.env", "token=1")
        .create()
        .await
        .expect("commit");
    let err = c.land("main", &bad.id).await.expect_err("rejected");
    assert_eq!(err.reason(), Some(ErrorReason::PreLandRejected));
    assert!(err.to_string().contains("secrets not allowed"), "{err}");
    let err = c.create_ref("leak", &bad.id).await.expect_err("branch rejected too");
    assert_eq!(err.reason(), Some(ErrorReason::PreLandRejected));
    assert_eq!(
        c.resolve_ref("main").await.expect("main").map(|t| t.changeset),
        Some(ok.id)
    );

    // 指向已有内容的分支和回退：没有新提交，不调用检查服务
    let before = *calls.lock().expect("lock");
    c.create_ref("copy", &ok.id).await.expect("existing content");
    c.update_ref("copy", &ok.id, None).await.expect("same target");
    assert_eq!(*calls.lock().expect("lock"), before);
}
