//! Integration: spawn a PTY with a proxy attached. Verify env
//! injection reaches the child AND that traffic from the child
//! through the proxy produces ProxyRecord events.
//!
//! The child for the test is `sh -c 'echo $ANTHROPIC_BASE_URL; curl ...'`:
//! - `echo` proves the daemon injected the proxy URL.
//! - `curl` proves a real request against the injected URL routes
//!   through the proxy into our mock upstream and emits a record.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use pilot_v2_daemon::agent_spawn::{
    spawn_with_proxy, AgentSpawnConfig, ProxyProvider, ProxyTarget,
};
use portable_pty::PtySize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn default_size() -> PtySize {
    PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

// Mock upstream shared with proxy tests (inline; simple enough).

struct UpstreamHandle {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    received: Arc<AtomicUsize>,
}

impl UpstreamHandle {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_upstream() -> UpstreamHandle {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_c = counter.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut rx => return,
                accept = listener.accept() => {
                    let Ok((stream, _)) = accept else { continue };
                    let counter = counter_c.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let counter = counter.clone();
                            async move {
                                counter.fetch_add(1, Ordering::SeqCst);
                                let _ = req.into_body().collect().await;
                                Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .body(Full::new(Bytes::from_static(b"{}")))
                                        .unwrap(),
                                )
                            }
                        });
                        let _ = http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            }
        }
    });
    UpstreamHandle {
        addr,
        shutdown: Some(tx),
        received: counter,
    }
}

// ── Env injection ──────────────────────────────────────────────────────

#[tokio::test]
async fn proxy_base_url_is_injected_into_spawn_env() {
    let upstream = spawn_upstream().await;
    let config = AgentSpawnConfig {
        session_key: "github:o/r#1".into(),
        argv: vec![
            "sh".into(),
            "-c".into(),
            "echo ANTHROPIC=$ANTHROPIC_BASE_URL".into(),
        ],
        cwd: None,
        size: default_size(),
        extra_env: HashMap::new(),
        proxy: Some(ProxyTarget {
            provider: ProxyProvider::Anthropic,
            upstream: upstream.url(),
        }),
    };
    let spawn = spawn_with_proxy(config).await.expect("spawn");

    // The child writes ANTHROPIC=http://127.0.0.1:PORT into the PTY
    // before exiting. Drain until we see it.
    let sub = spawn.pty.subscribe().await;
    // Wait for exit — deterministic.
    let _ = tokio::time::timeout(Duration::from_secs(5), spawn.pty.wait_exit()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replay = String::from_utf8_lossy(&spawn.pty.subscribe().await.replay).to_string();
    assert!(
        replay.contains("ANTHROPIC=http://127.0.0.1:"),
        "env var not injected; got: {replay}"
    );
    drop(sub);

    // Clean up.
    if let Some(proxy) = spawn.proxy {
        proxy.shutdown().await;
    }
    upstream.shutdown().await;
}

#[tokio::test]
async fn openai_provider_injects_openai_base_url() {
    let upstream = spawn_upstream().await;
    let config = AgentSpawnConfig {
        session_key: "o/r#2".into(),
        argv: vec![
            "sh".into(),
            "-c".into(),
            "echo OPENAI=$OPENAI_BASE_URL".into(),
        ],
        cwd: None,
        size: default_size(),
        extra_env: HashMap::new(),
        proxy: Some(ProxyTarget {
            provider: ProxyProvider::OpenAI,
            upstream: upstream.url(),
        }),
    };
    let spawn = spawn_with_proxy(config).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), spawn.pty.wait_exit()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replay = String::from_utf8_lossy(&spawn.pty.subscribe().await.replay).to_string();
    assert!(replay.contains("OPENAI=http://127.0.0.1:"), "got: {replay}");

    if let Some(p) = spawn.proxy {
        p.shutdown().await;
    }
    upstream.shutdown().await;
}

// ── No proxy for shell/log spawns ──────────────────────────────────────

#[tokio::test]
async fn no_proxy_means_no_proxy_env_vars() {
    let config = AgentSpawnConfig {
        session_key: "o/r#3".into(),
        argv: vec!["sh".into(), "-c".into(), "echo -n '|'$ANTHROPIC_BASE_URL'|'".into()],
        cwd: None,
        size: default_size(),
        extra_env: HashMap::new(),
        proxy: None,
    };
    let spawn = spawn_with_proxy(config).await.unwrap();
    assert!(spawn.proxy.is_none());
    assert!(spawn.records.is_none());

    let _ = tokio::time::timeout(Duration::from_secs(5), spawn.pty.wait_exit()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let replay = String::from_utf8_lossy(&spawn.pty.subscribe().await.replay).to_string();
    // The env var wasn't set, so the expansion is empty: "||".
    assert!(replay.contains("||"), "expected empty expansion; got: {replay}");
}

// ── End-to-end: child makes HTTP request through proxy ─────────────────

#[tokio::test]
async fn child_request_through_proxy_emits_record() {
    // Skip if curl isn't available — CI environments might not have it.
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: curl not available");
        return;
    }

    let upstream = spawn_upstream().await;
    let upstream_received = upstream.received.clone();

    let script = "curl -s -X POST $ANTHROPIC_BASE_URL/v1/messages \
        -H 'X-Pilot-Session: ent-test' \
        -H 'content-type: application/json' \
        -d '{\"model\":\"claude-sonnet-4-6\"}'";
    let config = AgentSpawnConfig {
        session_key: "github:o/r#ent".into(),
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: None,
        size: default_size(),
        extra_env: HashMap::new(),
        proxy: Some(ProxyTarget {
            provider: ProxyProvider::Anthropic,
            upstream: upstream.url(),
        }),
    };
    let spawn = spawn_with_proxy(config).await.unwrap();
    let mut records = spawn.records.expect("proxy has records channel");

    // Wait for the child to make its request and exit.
    let _ = tokio::time::timeout(Duration::from_secs(10), spawn.pty.wait_exit()).await;

    // The upstream should have seen exactly one request.
    assert_eq!(
        upstream_received.load(Ordering::SeqCst),
        1,
        "upstream saw one request via proxy"
    );

    // And we should have gotten one ProxyRecord.
    let rec = tokio::time::timeout(Duration::from_secs(2), records.recv())
        .await
        .expect("timeout")
        .expect("record");
    assert_eq!(rec.endpoint, "/v1/messages");
    assert_eq!(rec.status, 200);
    assert_eq!(rec.session_key.as_str(), "ent-test");

    if let Some(p) = spawn.proxy {
        p.shutdown().await;
    }
    upstream.shutdown().await;
}

// ── Records forward cleanly through the IPC Event layer ────────────────

#[tokio::test]
async fn proxy_record_event_round_trips_through_ipc() {
    // Assemble a ProxyRecord, wrap it in Event::ProxyRecord, send it
    // through the channel transport, assert we get it back intact.
    use pilot_v2_ipc::{Event, channel};

    let record = pilot_v2_llm_proxy::ProxyRecord {
        session_key: "github:o/r#1".into(),
        started_at: chrono::Utc::now(),
        duration: Duration::from_millis(420),
        provider: pilot_v2_llm_proxy::ApiProvider::Anthropic,
        endpoint: "/v1/messages".into(),
        request_model: Some("claude-sonnet-4-6".into()),
        tokens_input: Some(1024),
        tokens_output: Some(512),
        tokens_cache_read: None,
        tokens_cache_create: None,
        estimated_cost_usd: Some(0.01),
        tool_calls: vec![],
        assistant_text: Some("hi".into()),
        status: 200,
        error: None,
    };
    let event = Event::ProxyRecord(record.clone());

    let (mut client, server) = channel::pair();
    // Daemon-side pushes the event; client-side receives it.
    server.tx.send(event.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), client.recv())
        .await
        .expect("timeout")
        .expect("event");
    match received {
        Event::ProxyRecord(r) => {
            assert_eq!(format!("{r:?}"), format!("{record:?}"));
        }
        other => panic!("expected ProxyRecord, got {other:?}"),
    }
}
