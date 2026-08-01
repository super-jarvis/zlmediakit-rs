use md5::Md5;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// Shared authentication settings, passed to every protocol session.
#[derive(Debug, Clone)]
pub struct StreamAuth {
    pub enabled: bool,
    pub secret: String,
    /// Optional static realm used as a fallback when no `on_rtsp_realm` hook
    /// is configured (or it returns nothing). When set, RTSP Digest auth can
    /// always issue a `WWW-Authenticate: Digest` challenge even without a hook.
    pub default_realm: Option<String>,
    /// Optional RTSP Digest user table (`username -> password`). When present,
    /// a Digest response is validated against the password of the claimed
    /// `username`; if the username is unknown the shared `secret` is used as a
    /// fallback password (so single-secret deployments keep working).
    pub users: HashMap<String, String>,
}

impl StreamAuth {
    pub fn new(enabled: bool, secret: String) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            secret,
            default_realm: None,
            users: HashMap::new(),
        })
    }

    /// Like [`StreamAuth::new`] but with a static default realm for RTSP Digest
    /// auth fallback.
    pub fn new_with_realm(
        enabled: bool,
        secret: String,
        default_realm: Option<String>,
        users: HashMap<String, String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            secret,
            default_realm,
            users,
        })
    }

    /// Resolves the Digest password for a claimed `username`: the per-user
    /// password when configured, otherwise the shared `secret`.
    fn digest_password(&self, username: &str) -> String {
        self.users
            .get(username)
            .cloned()
            .unwrap_or_else(|| self.secret.clone())
    }

    /// Validates a `sign` query/parameter for a publish or play request.
    ///
    /// The expected signature is `sha256(secret | vhost | app | stream | action)`
    /// where `action` is `"pub"` or `"play"`. When auth is disabled, every
    /// request is allowed (and a missing/empty sign is tolerated).
    pub fn check(&self, vhost: &str, app: &str, stream: &str, action: &str, sign: &str) -> bool {
        if !self.enabled {
            return true;
        }
        if sign.is_empty() {
            return false;
        }
        validate_sign(&self.secret, sign, &[vhost, app, stream, action])
    }

    /// Validates an RTSP `Digest` `Authorization` header (RFC 2617/2617-style).
    ///
    /// The server acts as the HTTP Digest entity: the shared secret is used as
    /// the password, so `HA1 = md5(username:realm:secret)`. The client computes
    /// `response = md5(HA1:nonce:md5(method:uri))`. Any username is accepted as
    /// long as the response matches (the secret is the only credential).
    ///
    /// Returns `true` when `header` is a well-formed Digest response whose
    /// `realm`/`nonce`/`uri`/`method` produce the expected `response` for the
    /// configured `secret`.
    pub fn check_digest(&self, realm: &str, method: &str, uri: &str, header: &str) -> bool {
        let header = header.trim();
        let lower = header.to_ascii_lowercase();
        let rest = match lower.strip_prefix("digest ") {
            Some(r) => r,
            None => return false,
        };

        let mut fields: HashMap<String, String> = HashMap::new();
        for part in rest.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().trim_matches('"').to_string();
                fields.insert(key, val);
            }
        }

        let hdr_realm = match fields.get("realm") {
            Some(r) => r,
            None => return false,
        };
        if hdr_realm != realm {
            return false;
        }
        let nonce = match fields.get("nonce") {
            Some(n) => n,
            None => return false,
        };
        let hdr_uri = match fields.get("uri") {
            Some(u) => u,
            None => return false,
        };
        let response = match fields.get("response") {
            Some(r) => r,
            None => return false,
        };

        let username = fields.get("username").cloned().unwrap_or_default();
        let password = self.digest_password(&username);
        let ha1 = md5_hex(&format!("{}:{}:{}", username, realm, password));
        let ha2 = md5_hex(&format!("{}:{}", method, uri));
        let expected = md5_hex(&format!("{}:{}:{}", ha1, nonce, ha2));

        // Optional qop=auth integrity check (RFC 2617).
        if let Some(qop) = fields.get("qop") {
            if qop == "auth" {
                if let (Some(nc), Some(cnonce)) = (fields.get("nc"), fields.get("cnonce")) {
                    let ha2_alt = md5_hex(&format!("{}:{}", method, hdr_uri));
                    let expected_alt = md5_hex(&format!(
                        "{}:{}:{}:{}:{}:{}",
                        ha1, nonce, nc, cnonce, "auth", ha2_alt
                    ));
                    return response == &expected_alt;
                }
                return false;
            }
        }

        // Even when qop is absent, compare against the uri the server saw.
        let _ = hdr_uri;
        response == &expected
    }
}

/// Computes `md5(input)` as a lowercase hex string.
pub fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Computes `sha256(secret | part0 | part1 | ...)` as a lowercase hex string.
pub fn generate_sign(secret: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    for p in parts {
        hasher.update(b"|");
        hasher.update(p.as_bytes());
    }
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Constant-time signature comparison.
pub fn validate_sign(secret: &str, sign: &str, parts: &[&str]) -> bool {
    let expected = generate_sign(secret, parts);
    if expected.len() != sign.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, b) in expected.bytes().zip(sign.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}
