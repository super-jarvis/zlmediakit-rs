//! Tests for `StreamAuth::check_digest` — the RTSP Digest authentication
//! used together with the `on_rtsp_realm` hook.

use std::sync::Arc;
use zlmediakit_core::auth::{md5_hex, StreamAuth};

fn auth(secret: &str) -> Arc<StreamAuth> {
    StreamAuth::new(true, secret.to_string())
}

/// Compute the client-side Digest response exactly like a real RTSP client.
///
/// `HA1 = md5(username:realm:secret)`, `HA2 = md5(method:uri)`,
/// `response = md5(HA1:nonce:HA2)`.
fn client_response(
    username: &str,
    realm: &str,
    secret: &str,
    method: &str,
    uri: &str,
    nonce: &str,
) -> String {
    let ha1 = md5_hex(&format!("{}:{}:{}", username, realm, secret));
    let ha2 = md5_hex(&format!("{}:{}", method, uri));
    md5_hex(&format!("{}:{}:{}", ha1, nonce, ha2))
}

fn digest_header(
    username: &str,
    realm: &str,
    secret: &str,
    method: &str,
    uri: &str,
    nonce: &str,
) -> String {
    let response = client_response(username, realm, secret, method, uri, nonce);
    format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
        username, realm, nonce, uri, response
    )
}

#[test]
fn valid_digest_passes() {
    let secret = "mysecret";
    let realm = "zlmediakit";
    let nonce = "abc123";
    let uri = "rtsp://127.0.0.1/live/stream";
    let a = auth(secret);
    let hdr = digest_header("stream", realm, secret, "DESCRIBE", uri, nonce);
    assert!(a.check_digest(realm, "DESCRIBE", uri, &hdr));
}

#[test]
fn wrong_secret_fails() {
    let a = auth("rightsecret");
    // client computed response with a different secret
    let hdr = digest_header(
        "stream",
        "zlmediakit",
        "wrongsecret",
        "DESCRIBE",
        "rtsp://x/live/s",
        "n1",
    );
    assert!(!a.check_digest("zlmediakit", "DESCRIBE", "rtsp://x/live/s", &hdr));
}

#[test]
fn wrong_realm_fails() {
    let secret = "s";
    let a = auth(secret);
    // server expects "realmA" but header says "realmB"
    let hdr = digest_header("u", "realmB", secret, "DESCRIBE", "rtsp://x/live/s", "n1");
    assert!(!a.check_digest("realmA", "DESCRIBE", "rtsp://x/live/s", &hdr));
}

#[test]
fn wrong_uri_fails() {
    // Client signs a different URI than the one the server sees.
    let secret = "s";
    let realm = "r";
    let nonce = "n";
    let a = auth(secret);
    let hdr = digest_header("u", realm, secret, "DESCRIBE", "rtsp://x/live/wrong", nonce);
    assert!(!a.check_digest(realm, "DESCRIBE", "rtsp://x/live/s", &hdr));
}

#[test]
fn wrong_method_fails() {
    let secret = "s";
    let realm = "r";
    let nonce = "n";
    let uri = "rtsp://x/live/s";
    let a = auth(secret);
    // header computed for DESCRIBE, server checks PLAY
    let hdr = digest_header("u", realm, secret, "DESCRIBE", uri, nonce);
    assert!(!a.check_digest(realm, "PLAY", uri, &hdr));
}

#[test]
fn non_digest_header_rejected() {
    let a = auth("s");
    assert!(!a.check_digest("r", "DESCRIBE", "rtsp://x", "Basic YWJjOmRlZg=="));
    assert!(!a.check_digest("r", "DESCRIBE", "rtsp://x", "garbage"));
}

#[test]
fn missing_fields_rejected() {
    let a = auth("s");
    let partial = "Digest username=\"u\", realm=\"r\"";
    assert!(!a.check_digest("r", "DESCRIBE", "rtsp://x", partial));
}

#[test]
fn digest_with_qop_auth_passes() {
    let secret = "s";
    let realm = "r";
    let nonce = "n";
    let uri = "rtsp://x/live/s";
    let a = auth(secret);
    let ha1 = md5_hex(&format!("{}:{}:{}", "u", realm, secret));
    let ha2 = md5_hex(&format!("{}:{}", "PLAY", uri));
    let nc = "00000001";
    let cnonce = "cnonce-xyz";
    let response = md5_hex(&format!(
        "{}:{}:{}:{}:{}:{}",
        ha1, nonce, nc, cnonce, "auth", ha2
    ));
    let hdr = format!(
        "Digest username=\"u\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", qop=auth, nc={}, cnonce=\"{}\", response=\"{}\"",
        realm, nonce, uri, nc, cnonce, response
    );
    assert!(a.check_digest(realm, "PLAY", uri, &hdr));
}

/// Per-user password table: a user authenticates with their own password, not
/// the shared secret.
#[test]
fn digest_uses_per_user_password() {
    use std::collections::HashMap;
    let realm = "zlmediakit";
    let nonce = "n";
    let uri = "rtsp://x/live/s";
    let mut users = HashMap::new();
    users.insert("alice".to_string(), "alice-pass".to_string());
    users.insert("bob".to_string(), "bob-pass".to_string());
    let a = StreamAuth::new_with_realm(
        true,
        "shared-secret".to_string(),
        Some(realm.to_string()),
        users,
    );

    // Alice with her own password -> passes.
    let hdr_alice = digest_header("alice", realm, "alice-pass", "DESCRIBE", uri, nonce);
    assert!(a.check_digest(realm, "DESCRIBE", uri, &hdr_alice));

    // Bob with his own password -> passes.
    let hdr_bob = digest_header("bob", realm, "bob-pass", "DESCRIBE", uri, nonce);
    assert!(a.check_digest(realm, "DESCRIBE", uri, &hdr_bob));

    // Alice using the shared secret instead of her password -> fails.
    let hdr_wrong = digest_header("alice", realm, "shared-secret", "DESCRIBE", uri, nonce);
    assert!(!a.check_digest(realm, "DESCRIBE", uri, &hdr_wrong));

    // Unknown user falls back to the shared secret as password.
    let hdr_fallback = digest_header("carol", realm, "shared-secret", "DESCRIBE", uri, nonce);
    assert!(a.check_digest(realm, "DESCRIBE", uri, &hdr_fallback));
}
