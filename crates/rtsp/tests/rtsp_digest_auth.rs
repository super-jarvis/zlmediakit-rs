//! End-to-end test for RTSP Digest authentication driven by the
//! `on_rtsp_realm` hook.
//!
//! When a hook returns a realm for `on_rtsp_realm`, the RTSP server must
//! reject an unauthenticated DESCRIBE with `401` carrying a
//! `WWW-Authenticate: Digest` challenge. A client that computes the correct
//! Digest `Authorization` header must then pass the auth gate.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Duration;
use zlmediakit_core::auth::{md5_hex, StreamAuth};
use zlmediakit_core::config::HookConfig;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::RtspServer;

const RTSP_PORT: u16 = 19146;
const HOOK_PORT: u16 = 19147;

/// A tiny HTTP server that always answers
/// `{"code":0,"realm":"<REALM>"}` for the realm hook.
async fn spawn_realm_hook(realm: &'static str, port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            // Drain the client request so the socket closes cleanly (avoids
            // TCP RST that would abort the hook caller's read).
            let mut junk = [0u8; 4096];
            let _ = sock.read(&mut junk).await;
            let body = format!("{{\"code\":0,\"realm\":\"{}\"}}", realm);
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
}

async fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    loop {
        let s = String::from_utf8_lossy(buf);
        if let Some(pos) = s.find("\r\n\r\n") {
            let header_end = pos + 4;
            let content_length = s[..pos]
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= header_end + content_length {
                return;
            }
        }
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn rtsp_request(stream: &mut TcpStream, request: &[u8], buf: &mut Vec<u8>) -> (u16, String) {
    stream.write_all(request).await.unwrap();
    read_response(stream, buf).await;
    let s = String::from_utf8_lossy(buf);
    let pos = s.find("\r\n\r\n").unwrap_or(0);
    let header_end = pos + 4;
    let status_line = s.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let content_length = s[..pos]
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total_len = header_end + content_length;
    let resp_str = s[..total_len].to_string();
    buf.drain(..total_len);
    (code, resp_str)
}

fn header_value(resp: &str, name: &str) -> Option<String> {
    resp.lines().find_map(|l| {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
        None
    })
}

fn extract_digest(resp: &str) -> (String, String) {
    let www = header_value(resp, "WWW-Authenticate").expect("WWW-Authenticate header missing");
    let realm = www
        .split(',')
        .find_map(|p| p.trim().strip_prefix("realm=").or_else(|| p.trim().strip_prefix("Digest realm=")))
        .map(|v| v.trim_matches('"').to_string())
        .expect("realm not found");
    let nonce = www
        .split(',')
        .find_map(|p| p.trim().strip_prefix("nonce="))
        .map(|v| v.trim_matches('"').to_string())
        .expect("nonce not found");
    (realm, nonce)
}

fn make_digest_auth(username: &str, realm: &str, secret: &str, method: &str, uri: &str, nonce: &str) -> String {
    let ha1 = md5_hex(&format!("{}:{}:{}", username, realm, secret));
    let ha2 = md5_hex(&format!("{}:{}", method, uri));
    let response = md5_hex(&format!("{}:{}:{}", ha1, nonce, ha2));
    format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
        username, realm, nonce, uri, response
    )
}

#[tokio::test]
async fn rtsp_digest_auth_flow() {
    let realm = "edge-realm";
    let secret = "hook-secret";
    spawn_realm_hook(realm, HOOK_PORT).await;

    let mgr = Arc::new(MediaSourceManager::new(None));
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(true, secret.to_string());

    let hook_cfg = HookConfig {
        on_rtsp_realm: Some(format!("http://127.0.0.1:{}/index/api/realm", HOOK_PORT)),
        ..Default::default()
    };
    let hook = HookClient::new(hook_cfg);

    let addr = format!("127.0.0.1:{}", RTSP_PORT);
    let srv = RtspServer::new(&addr, mgr.clone(), event_bus, auth, Some(hook), None, None)
        .await
        .expect("RtspServer should start");
    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let app = "live";
    let stream_name = "test";
    let uri = format!("rtsp://127.0.0.1:{}/{}/{}", RTSP_PORT, app, stream_name);

    // --- First DESCRIBE without credentials -> 401 + Digest challenge ---
    let mut sock = TcpStream::connect(("127.0.0.1", RTSP_PORT)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();
    let describe_no_auth = format!(
        "DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n",
        uri
    );
    let (code1, resp1) = rtsp_request(&mut sock, describe_no_auth.as_bytes(), &mut buf).await;
    assert_eq!(code1, 401, "unauthenticated DESCRIBE must be 401");
    let www = header_value(&resp1, "WWW-Authenticate").expect("must carry WWW-Authenticate");
    assert!(www.starts_with("Digest"), "must be a Digest challenge: {}", www);
    let (chal_realm, nonce) = extract_digest(&resp1);
    assert_eq!(chal_realm, realm, "realm must come from on_rtsp_realm hook");

    // --- Second DESCRIBE with correct Digest credentials -> passes auth ---
    let authorization = make_digest_auth(stream_name, &chal_realm, secret, "DESCRIBE", &uri, &nonce);
    let describe_auth = format!(
        "DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
        uri, authorization
    );
    let (code2, _) = rtsp_request(&mut sock, describe_auth.as_bytes(), &mut buf).await;
    // Stream does not exist, so we expect 404 (not 401). The important part is
    // that the auth gate is passed.
    assert_ne!(code2, 401, "valid Digest credentials must pass the auth gate");
    assert!(code2 == 404 || code2 == 200, "expected 404 (no such stream) but got {}", code2);
}

#[tokio::test]
async fn rtsp_digest_rejects_wrong_response() {
    let realm = "edge-realm";
    let secret = "hook-secret";
    let hook_port = 19149u16;
    let rtsp_port = 19148u16;
    spawn_realm_hook(realm, hook_port).await;

    let mgr = Arc::new(MediaSourceManager::new(None));
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(true, secret.to_string());
    let hook_cfg = HookConfig {
        on_rtsp_realm: Some(format!("http://127.0.0.1:{}/index/api/realm", hook_port)),
        ..Default::default()
    };
    let hook = HookClient::new(hook_cfg);

    let addr = format!("127.0.0.1:{}", rtsp_port);
    let srv = RtspServer::new(&addr, mgr.clone(), event_bus, auth, Some(hook), None, None)
        .await
        .expect("RtspServer should start");
    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let uri = format!("rtsp://127.0.0.1:{}/live/test", rtsp_port);
    let mut sock = TcpStream::connect(("127.0.0.1", rtsp_port)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();
    let (code1, resp1) = rtsp_request(
        &mut sock,
        format!("DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n", uri).as_bytes(),
        &mut buf,
    )
    .await;
    assert_eq!(code1, 401);
    let (chal_realm, nonce) = extract_digest(&resp1);

    // Wrong secret -> wrong response.
    let authorization = make_digest_auth("test", &chal_realm, "wrong-secret", "DESCRIBE", &uri, &nonce);
    let (code2, _) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
            uri, authorization
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_eq!(code2, 401, "wrong Digest response must be rejected with 401");
}
