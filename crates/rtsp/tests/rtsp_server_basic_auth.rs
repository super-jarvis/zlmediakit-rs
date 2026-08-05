//! End-to-end tests for RTSP server-side Basic authentication (RFC 7617).
//!
//! When auth is enabled and a realm is known, the RTSP server must reject an
//! unauthenticated DESCRIBE with `401` carrying a `WWW-Authenticate` challenge
//! that advertises the `Basic` scheme. A client that retries with a correct
//! `Authorization: Basic base64(user:password)` header must pass the auth
//! gate; wrong credentials must keep failing with `401`.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::RtspServer;

const RTSP_PORT: u16 = 19153;

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

fn basic_authorization(username: &str, password: &str) -> String {
    let token = BASE64.encode(format!("{username}:{password}"));
    format!("Basic {token}")
}

async fn spawn_server(
    rtsp_port: u16,
    secret: &str,
    realm: Option<&str>,
    users: HashMap<String, String>,
) -> u16 {
    let mgr = Arc::new(MediaSourceManager::new(None));
    let event_bus = Arc::new(EventBus::new(1024));
    let auth =
        StreamAuth::new_with_realm(true, secret.to_string(), realm.map(str::to_string), users);
    let addr = format!("127.0.0.1:{}", rtsp_port);
    let srv = RtspServer::new(&addr, mgr.clone(), event_bus, auth, None, None, None)
        .await
        .expect("RtspServer should start");
    let port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    port
}

#[tokio::test]
async fn rtsp_server_basic_issues_challenge_and_validates_credentials() {
    let realm = "edge-realm";
    let secret = "server-secret";
    let users = HashMap::from([("alice".to_string(), "wonder".to_string())]);
    let port = spawn_server(RTSP_PORT, secret, Some(realm), users).await;

    let uri = format!("rtsp://127.0.0.1:{}/live/test", port);
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();

    // --- First DESCRIBE without credentials -> 401 + Basic challenge ---
    let describe_no_auth = format!(
        "DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n",
        uri
    );
    let (code1, resp1) = rtsp_request(&mut sock, describe_no_auth.as_bytes(), &mut buf).await;
    assert_eq!(code1, 401, "unauthenticated DESCRIBE must be 401");
    let www = header_value(&resp1, "WWW-Authenticate").expect("must carry WWW-Authenticate");
    assert!(
        www.to_ascii_lowercase().contains("basic"),
        "must advertise the Basic scheme: {}",
        www
    );
    assert!(
        www.contains(realm),
        "Basic challenge must carry the realm: {}",
        www
    );

    // --- Second DESCRIBE with correct Basic credentials -> passes auth ---
    let auth_header = basic_authorization("alice", "wonder");
    let describe_auth = format!(
        "DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
        uri, auth_header
    );
    let (code2, _) = rtsp_request(&mut sock, describe_auth.as_bytes(), &mut buf).await;
    // Stream does not exist, so we expect 404 (not 401). The important part is
    // that the auth gate is passed.
    assert_ne!(
        code2, 401,
        "valid Basic credentials must pass the auth gate"
    );
    assert!(
        code2 == 404 || code2 == 200,
        "expected 404 (no such stream) but got {}",
        code2
    );
}

#[tokio::test]
async fn rtsp_server_basic_rejects_wrong_password() {
    let realm = "edge-realm";
    let secret = "server-secret";
    let users = HashMap::from([("alice".to_string(), "wonder".to_string())]);
    let port = spawn_server(RTSP_PORT + 1, secret, Some(realm), users).await;

    let uri = format!("rtsp://127.0.0.1:{}/live/test", port);
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();

    let (code1, _) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n",
            uri
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_eq!(code1, 401);

    // Wrong password -> still 401.
    let auth_header = basic_authorization("alice", "wrong");
    let (code2, _) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
            uri, auth_header
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_eq!(code2, 401, "wrong Basic password must be rejected");

    // Unknown username -> falls back to the shared secret.
    let auth_header = basic_authorization("bob", secret);
    let (code3, _) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 3\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
            uri, auth_header
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_ne!(code3, 401, "secret fallback must pass for unknown users");
    assert!(
        code3 == 404 || code3 == 200,
        "expected 404 (no such stream) but got {}",
        code3
    );
}

#[tokio::test]
async fn rtsp_server_basic_fallback_default_realm() {
    let realm = "zlmediakit";
    let secret = "fallback-secret";
    // No per-user table; the shared secret is the only credential.
    let users = HashMap::new();
    let port = spawn_server(RTSP_PORT + 2, secret, Some(realm), users).await;

    let uri = format!("rtsp://127.0.0.1:{}/live/test", port);
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sock.set_nodelay(true).unwrap();
    let mut buf = Vec::new();

    let (code1, resp1) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n",
            uri
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_eq!(code1, 401, "anonymous DESCRIBE must be rejected with 401");
    let www = header_value(&resp1, "WWW-Authenticate").expect("must carry a challenge");
    assert!(
        www.to_ascii_lowercase().contains("basic"),
        "must advertise the Basic scheme: {}",
        www
    );

    // Correct shared secret (any username) -> passes the auth gate.
    let auth_header = basic_authorization("anonymous", secret);
    let (code2, _) = rtsp_request(
        &mut sock,
        format!(
            "DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nAuthorization: {}\r\n\r\n",
            uri, auth_header
        )
        .as_bytes(),
        &mut buf,
    )
    .await;
    assert_ne!(code2, 401, "shared secret must pass the auth gate");
    assert!(
        code2 == 404 || code2 == 200,
        "expected 404 (no such stream) but got {}",
        code2
    );
}
