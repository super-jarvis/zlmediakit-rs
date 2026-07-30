//! External authentication hook client.
//!
//! Sends HTTP POST callbacks to an external service to authorize
//! publish/play operations. Follows the ZLMediaKit hook protocol:
//! the server POSTs form-encoded parameters (`vhost`, `app`, `stream`,
//! `params`) and the external service responds with JSON:
//! `{"code": 0}` to allow, `{"code": -1, "msg": "reason"}` to deny.
//!
//! When no hook URL is configured or on network/timeout errors the
//! behaviour is **fail-open** (allow).

use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

pub use crate::config::HookConfig;

/// The result of a hook callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// The external service allowed the operation.
    Allow,
    /// The external service denied the operation with a reason.
    Deny(String),
}

/// Stateless HTTP client that calls external hook URLs.
pub struct HookClient {
    config: HookConfig,
}

impl HookClient {
    /// Builds a client from a `HookConfig` (typically from config file).
    pub fn new(config: HookConfig) -> Arc<Self> {
        Arc::new(Self { config })
    }

    /// Returns a client with no hooks configured — always allows.
    /// Convenient for tests that don't need hook callbacks.
    pub fn empty() -> Arc<Self> {
        Self::new(HookConfig::default())
    }

    /// Returns `true` when no hook URLs are configured.
    pub fn is_empty(&self) -> bool {
        self.config.on_publish.is_none()
            && self.config.on_play.is_none()
            && self.config.on_stream_not_found.is_none()
    }

    // ── public hook entry-points ──────────────────────────────────────

    /// Called before a stream starts publishing.
    /// `params` carries any query-string parameters from the original
    /// client request (e.g. `sign=...&token=...`).
    pub async fn on_publish(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
        params: &str,
    ) -> HookResult {
        let url = match &self.config.on_publish {
            Some(u) => u,
            None => return HookResult::Allow,
        };
        self.call_hook(url, vhost, app, stream, params).await
    }

    /// Called before a player starts consuming a stream.
    pub async fn on_play(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
        params: &str,
    ) -> HookResult {
        let url = match &self.config.on_play {
            Some(u) => u,
            None => return HookResult::Allow,
        };
        self.call_hook(url, vhost, app, stream, params).await
    }

    /// Called when a player requests a stream that does not exist.
    pub async fn on_stream_not_found(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
    ) -> HookResult {
        let url = match &self.config.on_stream_not_found {
            Some(u) => u,
            None => return HookResult::Allow,
        };
        self.call_hook(url, vhost, app, stream, "").await
    }

    // ── internal helpers ───────────────────────────────────────────────

    /// Sends an HTTP POST to `url` with retries. Falls back to `Allow` on
    /// persistent errors (fail-open).
    async fn call_hook(
        &self,
        url: &str,
        vhost: &str,
        app: &str,
        stream: &str,
        params: &str,
    ) -> HookResult {
        for attempt in 0..=self.config.retry {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            match self.try_call(url, vhost, app, stream, params).await {
                Ok(result) => return result,
                Err(e) => {
                    tracing::warn!(
                        "Hook call attempt {}/{} to {} failed: {}",
                        attempt + 1,
                        self.config.retry + 1,
                        url,
                        e
                    );
                }
            }
        }
        tracing::warn!(
            "Hook call to {} exhausted all retries — falling back to allow",
            url
        );
        HookResult::Allow
    }

    /// Single hook attempt: connect, POST, read response, parse JSON.
    async fn try_call(
        &self,
        url: &str,
        vhost: &str,
        app: &str,
        stream: &str,
        params: &str,
    ) -> anyhow::Result<HookResult> {
        let parsed = parse_url(url)?;
        let body = format!(
            "vhost={}&app={}&stream={}&params={}",
            url_encode(vhost),
            url_encode(app),
            url_encode(stream),
            url_encode(params)
        );

        let request = format!(
            "POST {} HTTP/1.0\r\n\
             Host: {}\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            parsed.path, parsed.host, body.len(), body
        );

        let dur = Duration::from_secs(self.config.timeout_sec);
        let mut stream = timeout(dur, TcpStream::connect(parsed.addr.as_str())).await??;
        timeout(dur, stream.write_all(request.as_bytes())).await??;

        let mut response = Vec::new();
        timeout(dur, stream.read_to_end(&mut response)).await??;

        let response_str = String::from_utf8_lossy(&response);

        // Parse HTTP status line
        let first_line = response_str.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid HTTP response: no status line");
        }
        let status: u16 = parts[1]
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid HTTP status: {}", parts[1]))?;

        if status != 200 {
            anyhow::bail!("Hook returned HTTP {}", status);
        }

        // Extract body after the first empty line
        let body_start = response_str
            .find("\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(0);
        let json_body = response_str[body_start..].trim();

        #[derive(Deserialize)]
        struct HookResponse {
            code: i32,
            #[serde(default)]
            msg: String,
        }

        match serde_json::from_str::<HookResponse>(json_body) {
            Ok(resp) => {
                if resp.code == 0 {
                    Ok(HookResult::Allow)
                } else {
                    Ok(HookResult::Deny(resp.msg))
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Hook response parse error: {} — body = {:?}",
                    e,
                    json_body
                );
                // Unparseable response → fail-open.
                Ok(HookResult::Allow)
            }
        }
    }
}

// ── URL parsing ──────────────────────────────────────────────────────

struct ParsedUrl {
    host: String,
    addr: String,
    path: String,
}

fn parse_url(url: &str) -> anyhow::Result<ParsedUrl> {
    let url = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (host_port, path) = url.split_once('/').unwrap_or((url, "/"));
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(80)),
        None => (host_port.to_string(), 80),
    };
    Ok(ParsedUrl {
        host: host_port.to_string(),
        addr: format!("{}:{}", host, port),
        path: format!("/{}", path),
    })
}

// ── URL encoding ─────────────────────────────────────────────────────

fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_alphanumeric() {
        assert_eq!(url_encode("hello"), "hello");
        assert_eq!(url_encode("test_123"), "test_123");
        assert_eq!(url_encode("a.b-c~"), "a.b-c~");
    }

    #[test]
    fn url_encode_special() {
        let encoded = url_encode("hello world");
        assert_eq!(encoded, "hello%20world");
        let encoded = url_encode("测试");
        // UTF-8 encoded bytes get percent-encoded
        assert!(!encoded.contains('测'));
    }

    #[test]
    fn parse_url_simple() {
        let parsed = parse_url("http://127.0.0.1:8080/index/hook/on_publish").unwrap();
        assert_eq!(parsed.addr, "127.0.0.1:8080");
        assert_eq!(parsed.host, "127.0.0.1:8080");
        assert_eq!(parsed.path, "/index/hook/on_publish");
    }

    #[test]
    fn parse_url_default_port() {
        let parsed = parse_url("http://localhost/hook").unwrap();
        assert_eq!(parsed.addr, "localhost:80");
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.path, "/hook");
    }

    #[test]
    fn hook_client_empty_allows_all() {
        let client = HookClient::empty();
        assert!(client.is_empty());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(
            rt.block_on(client.on_publish("v", "a", "s", "")),
            HookResult::Allow
        );
        assert_eq!(
            rt.block_on(client.on_play("v", "a", "s", "")),
            HookResult::Allow
        );
        assert_eq!(
            rt.block_on(client.on_stream_not_found("v", "a", "s")),
            HookResult::Allow
        );
    }

    #[test]
    fn default_hook_config_is_empty() {
        let cfg = HookConfig::default();
        assert!(cfg.on_publish.is_none());
        assert!(cfg.on_play.is_none());
        assert!(cfg.on_stream_not_found.is_none());
        assert_eq!(cfg.timeout_sec, 5);
        assert_eq!(cfg.retry, 1);
    }

    #[test]
    fn hook_config_toml_deserialize() {
        let toml_str = r#"
on_publish = "http://127.0.0.1:8080/index/hook/on_publish"
on_play = "http://127.0.0.1:8080/index/hook/on_play"
timeout_sec = 3
retry = 2
"#;
        let cfg: HookConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.on_publish.unwrap(),
            "http://127.0.0.1:8080/index/hook/on_publish"
        );
        assert_eq!(cfg.timeout_sec, 3);
        assert_eq!(cfg.retry, 2);
    }
}
