//! End-to-end tests for external auth hook callbacks.
//!
//! Starts a mock HTTP server, creates a HookClient pointed at it, and
//! verifies that publish/play is allowed or denied based on the JSON
//! response `{"code": 0}` vs `{"code": 1}`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Duration;
use zlmediakit_core::hook::{HookClient, HookConfig, HookResult};

/// Spawns a single-use mock HTTP server on `port` that, when it receives
/// a POST, responds with `{"code": <code>}` and increments `call_count`.
async fn spawn_mock_hook_server(port: u16, code: u64, call_count: Arc<AtomicU64>) {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .expect("bind mock hook");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            call_count.fetch_add(1, Ordering::SeqCst);

            let body = format!("{{\"code\": {}}}\n", code);
            let response = format!(
                "HTTP/1.0 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
}

// ── tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn hook_allow_code_zero() {
    let port = 19601u16;
    let call_count = Arc::new(AtomicU64::new(0));
    spawn_mock_hook_server(port, 0, call_count.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_publish: Some(format!("http://127.0.0.1:{}/index/hook/on_publish", port)),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_publish("vhost", "live", "test", "").await;
    assert_eq!(result, HookResult::Allow);
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "hook should be called once");
}

#[tokio::test]
async fn hook_deny_code_nonzero() {
    let port = 19611u16;
    let call_count = Arc::new(AtomicU64::new(0));
    spawn_mock_hook_server(port, 1, call_count.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_publish: Some(format!("http://127.0.0.1:{}/index/hook/on_publish", port)),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_publish("vhost", "live", "test", "").await;
    assert!(
        matches!(&result, HookResult::Deny(_)),
        "expected Deny, got {:?}",
        result
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "hook should be called once");
}

#[tokio::test]
async fn hook_on_play_called() {
    let port = 19621u16;
    let call_count = Arc::new(AtomicU64::new(0));
    spawn_mock_hook_server(port, 0, call_count.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_play: Some(format!("http://127.0.0.1:{}/index/hook/on_play", port)),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_play("vhost", "live", "test", "sign=abc").await;
    assert_eq!(result, HookResult::Allow);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hook_empty_config_allows() {
    let hook = HookClient::empty();
    assert!(hook.is_empty());
    let result = hook.on_publish("v", "a", "s", "").await;
    assert_eq!(result, HookResult::Allow);
}

#[tokio::test]
async fn hook_timeout_fallthrough_allow() {
    // Point at a non-listening port → timeout → Allow (fail-open)
    let hook = HookClient::new(HookConfig {
        on_publish: Some("http://127.0.0.1:19999/index/hook/on_publish".to_string()),
        timeout_sec: 1,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_publish("vhost", "live", "test", "").await;
    assert_eq!(result, HookResult::Allow, "timeout should fallback to Allow");
}

#[tokio::test]
async fn hook_on_stream_not_found_called() {
    let port = 19631u16;
    let call_count = Arc::new(AtomicU64::new(0));
    spawn_mock_hook_server(port, 0, call_count.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_stream_not_found: Some(format!(
            "http://127.0.0.1:{}/index/hook/on_stream_not_found",
            port
        )),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_stream_not_found("vhost", "live", "test").await;
    assert_eq!(result, HookResult::Allow);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hook_selects_correct_url() {
    // When on_publish is configured but we call on_play, it should
    // fall back to Allow without calling the server.
    let port = 19641u16;
    let call_count = Arc::new(AtomicU64::new(0));
    spawn_mock_hook_server(port, 0, call_count.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_publish: Some(format!("http://127.0.0.1:{}/index/hook/on_publish", port)),
        ..HookConfig::default()
    });

    let result = hook.on_play("vhost", "live", "test", "").await;
    assert_eq!(result, HookResult::Allow);
    // on_play URL is not configured, so the server should NOT be called
    assert_eq!(call_count.load(Ordering::SeqCst), 0, "play hook should not call server when not configured");
}

#[tokio::test]
async fn hook_unparseable_response_defaults_allow() {
    let port = 19651u16;
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .expect("bind");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Send malformed response (not JSON)
            let response = "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nnot json";
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_publish: Some(format!("http://127.0.0.1:{}/hook", port)),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_publish("vhost", "live", "test", "").await;
    assert_eq!(
        result,
        HookResult::Allow,
        "unparseable response should default to Allow"
    );
}

#[tokio::test]
async fn hook_http_error_status_defaults_allow() {
    let port = 19661u16;
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .expect("bind");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response =
                "HTTP/1.0 500 Internal Server Error\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hook = HookClient::new(HookConfig {
        on_publish: Some(format!("http://127.0.0.1:{}/hook", port)),
        timeout_sec: 2,
        retry: 0,
        ..HookConfig::default()
    });

    let result = hook.on_publish("vhost", "live", "test", "").await;
    assert_eq!(
        result,
        HookResult::Allow,
        "HTTP 500 should default to Allow"
    );
}
