//! ref 变化的事件：合入后 webhook 收到带提交详情的事件，ListRefEvents 拉取到同样的变化。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use scion_api::events::{EventsConfig, WebhookConfig};
use scion_api::{App, ServerConfig, serve};
use scion_client::Client;
use scion_store::{MemStore, MetaStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 收到的 webhook 请求体。
type Hooks = Arc<Mutex<Vec<serde_json::Value>>>;

async fn webhook() -> (String, Hooks) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/hook", listener.local_addr().expect("addr"));
    let got: Hooks = Arc::default();
    let log = got.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
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
                    let body = buf[i + 4..i + 4 + len].to_vec();
                    buf.drain(..i + 4 + len);
                    if let Ok(v) = serde_json::from_slice(&body) {
                        log.lock().expect("lock").push(v);
                    }
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                }
            });
        }
    });
    (url, got)
}

#[tokio::test(flavor = "multi_thread")]
async fn lands_produce_events_for_webhooks_and_pollers() {
    let (hook_url, hooks) = webhook().await;
    let store = Arc::new(MemStore::new());
    store.create_repo("mono").await.expect("repo");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let app = App {
        config: Arc::new(ServerConfig {
            warm_record_cache: false,
            events: EventsConfig {
                webhook: Some(WebhookConfig {
                    url: hook_url,
                    secret: Some("s3cret".into()),
                    timeout: Duration::from_secs(5),
                }),
                nats: None,
                retention: Duration::from_secs(3600),
            },
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
    assert!(c.has_feature(scion_proto::features::EVENTS));
    let a = c
        .changeset(None)
        .put("README.md", "hello")
        .message("first")
        .create()
        .await
        .expect("commit");
    c.land("main", &a.id).await.expect("land");
    let b = c
        .changeset(Some(a.id))
        .put("src/lib.rs", "pub fn f() {}")
        .message("second")
        .create()
        .await
        .expect("commit 2");
    c.land("main", &b.id).await.expect("land 2");

    // 拉取：两次变化，按 seq 递增
    let page = c.list_ref_events(0, 10, Some("mono")).await.expect("poll");
    assert_eq!(page.events.len(), 2);
    assert_eq!((page.events[0].old, page.events[0].new), (None, Some(a.id)));
    assert_eq!((page.events[1].old, page.events[1].new), (Some(a.id), Some(b.id)));
    assert_eq!(page.next_seq, page.events[1].seq);
    assert!(!page.truncated);

    // 推送：带提交详情
    for _ in 0..100 {
        if hooks.lock().expect("lock").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = hooks.lock().expect("lock").clone();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0]["type"], "ref.created");
    assert_eq!(got[1]["type"], "ref.updated");
    assert_eq!(got[1]["repo"], "mono");
    assert_eq!(got[1]["commits"][0]["message"], "second");
    assert_eq!(got[1]["commits"][0]["changes"][0]["path"], "src/lib.rs");
    assert_eq!(got[1]["commits"].as_array().map(Vec::len), Some(1));

    // git 兼容 sha：事件中的与查询接口一致，且能直接当作版本号使用
    assert!(c.has_feature(scion_proto::features::GIT_SHA1));
    let shas = c.git_shas(&[a.id, b.id]).await.expect("git shas");
    assert_eq!(got[1]["old_git_sha"], shas[0]);
    assert_eq!(got[1]["new_git_sha"], shas[1]);
    assert_eq!(got[1]["commits"][0]["git_sha"], shas[1]);
    assert_eq!(got[0]["commits"][0]["git_sha"], shas[0]);
    assert!(got[0]["old_git_sha"].is_null());
    assert_eq!(c.git_sha("main").await.expect("derived"), Some(shas[1].clone()));
    assert_eq!(c.resolve_revision(&shas[0]).await.expect("resolve").id, a.id);
}
