use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Shared authentication settings, passed to every protocol session.
#[derive(Debug, Clone)]
pub struct StreamAuth {
    pub enabled: bool,
    pub secret: String,
}

impl StreamAuth {
    pub fn new(enabled: bool, secret: String) -> Arc<Self> {
        Arc::new(Self { enabled, secret })
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
