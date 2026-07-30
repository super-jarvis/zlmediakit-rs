use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_rtmp_port")]
    pub rtmp_port: u16,

    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,

    #[serde(default = "default_http_port")]
    pub http_port: u16,

    #[serde(default = "default_api_port")]
    pub api_port: u16,

    #[serde(default = "default_rtp_port")]
    pub rtp_port: u16,

    #[serde(default = "default_vhost")]
    pub default_vhost: String,

    #[serde(default = "default_secret")]
    pub secret: String,

    #[serde(default)]
    pub auth_enabled: bool,

    #[serde(default)]
    pub hook: HookConfig,

    #[serde(default)]
    pub general: GeneralConfig,

    #[serde(default)]
    pub rtmp: RtmpConfig,

    #[serde(default)]
    pub rtsp: RtspConfig,

    #[serde(default)]
    pub http: HttpConfig,

    #[serde(default)]
    pub record: RecordConfig,

    #[serde(default)]
    pub proxy: ProxyConfig,

    #[serde(default)]
    pub srt: SrtConfig,

    #[serde(default)]
    pub gb28181: Gb28181Config,

    #[serde(default)]
    pub cluster: ClusterConfig,

    #[serde(default)]
    pub webrtc: WebRtcConfig,

    #[serde(default = "default_webrtc_port")]
    pub webrtc_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_flow_threshold")]
    pub flow_threshold: i64,
    #[serde(default = "default_stream_none_reader_delay")]
    pub stream_none_reader_delay: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtmpConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default)]
    pub ssl_cert: Option<String>,
    #[serde(default)]
    pub ssl_key: Option<String>,
    #[serde(default)]
    pub ssl_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default)]
    pub ssl_cert: Option<String>,
    #[serde(default)]
    pub ssl_key: Option<String>,
    #[serde(default)]
    pub ssl_port: Option<u16>,
    #[serde(default = "default_true")]
    pub tcp_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default)]
    pub ssl_cert: Option<String>,
    #[serde(default)]
    pub ssl_key: Option<String>,
    #[serde(default)]
    pub ssl_port: Option<u16>,
    #[serde(default = "default_true")]
    pub dir_root: bool,
    #[serde(default = "default_www_root")]
    pub www_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    /// ICE server URLs, e.g. `stun:stun.l.google.com:19302` or
    /// `turn:turn.example.com:3478?transport=udp`.
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRtcConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    /// ICE servers (STUN/TURN) used for NAT traversal. Empty means host
    /// candidates only (same-LAN / open-Internet peers).
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEntryConfig {
    /// The remote URL to pull (e.g. `rtmp://192.168.1.100/live/camera1`).
    pub url: String,
    /// Virtual host to publish under locally.
    #[serde(default = "default_vhost")]
    pub vhost: String,
    /// Application name to publish under locally.
    #[serde(default = "default_proxy_app")]
    pub app: String,
    /// Stream name to publish under locally (defaults to the last path
    /// component of `url` when empty).
    pub stream: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Whether to auto-start proxy entries from this config section.
    #[serde(default)]
    pub enabled: bool,
    /// List of remote streams to pull on startup.
    #[serde(default)]
    pub pulls: Vec<ProxyEntryConfig>,
}

/// External authentication hook configuration (HTTP callbacks).
/// When a hook URL is configured, the server will POST to it before
/// allowing a publish or play operation. The external service responds
/// with JSON `{"code": 0}` to allow, or `{"code": -1, "msg": "..."}` to deny.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Called before a stream starts publishing.
    /// POST params: vhost, app, stream, params
    #[serde(default)]
    pub on_publish: Option<String>,

    /// Called before a stream starts playing.
    /// POST params: vhost, app, stream, params
    #[serde(default)]
    pub on_play: Option<String>,

    /// Called when a player requests a stream that does not exist.
    /// POST params: vhost, app, stream
    #[serde(default)]
    pub on_stream_not_found: Option<String>,

    /// Timeout in seconds for each HTTP hook call (default 5).
    #[serde(default = "default_hook_timeout")]
    pub timeout_sec: u64,

    /// Number of retries on network failure before falling back to allow
    /// (fail-open). Default is 1 (one retry after initial attempt).
    #[serde(default = "default_hook_retry")]
    pub retry: u32,
}

fn default_hook_timeout() -> u64 {
    5
}
fn default_hook_retry() -> u32 {
    1
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            on_publish: None,
            on_play: None,
            on_stream_not_found: None,
            timeout_sec: default_hook_timeout(),
            retry: default_hook_retry(),
        }
    }
}

/// SRT (Secure Reliable Transport) ingest configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrtConfig {
    /// Whether to enable the SRT ingest server.
    #[serde(default)]
    pub enabled: bool,
    /// Port to listen on (default: 9000).
    #[serde(default = "default_srt_port")]
    pub port: u16,
    /// Latency in milliseconds (default: 120).
    #[serde(default = "default_srt_latency")]
    pub latency_ms: u32,
    /// Optional encryption passphrase.
    #[serde(default)]
    pub passphrase: Option<String>,
}

/// GB28181 (SIP + RTP/PS) ingest configuration.
///
/// Enables a minimal SIP registrar/server (UDP) that accepts device
/// `REGISTER`/`MESSAGE` and can `INVITE` a device to push its PS stream
/// over RTP to a local media port. The RTP/PS stream is demuxed and
/// published as a `MediaSource` so it can be played over any protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gb28181Config {
    /// Whether to enable the GB28181 SIP + RTP server.
    #[serde(default)]
    pub enabled: bool,
    /// UDP port for the SIP signaling server (default: 5060).
    #[serde(default = "default_gb28181_sip_port")]
    pub sip_port: u16,
    /// Base UDP port for RTP media reception (auto-incremented per stream).
    #[serde(default = "default_gb28181_media_port")]
    pub media_port: u16,
    /// SIP realm used for digest authentication.
    #[serde(default = "default_gb28181_realm")]
    pub realm: String,
    /// SIP password for digest authentication. Empty disables auth.
    #[serde(default)]
    pub password: Option<String>,
    /// Externally reachable IP put into SDP `c=` lines so devices know where
    /// to send RTP. Auto-detected (first non-loopback interface) when empty.
    #[serde(default)]
    pub host: Option<String>,
}

/// Cluster peer configuration — push streams to a remote node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPeerConfig {
    /// Remote RTMP URL to push to (e.g. rtmp://192.168.1.10/live).
    pub url: String,
    /// Local vhost to relay.
    #[serde(default = "default_vhost")]
    pub vhost: String,
    /// Local app to relay (default: live).
    #[serde(default = "default_proxy_app")]
    pub app: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Whether cluster push relay is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// List of remote peers to push streams to.
    #[serde(default)]
    pub push_to: Vec<ClusterPeerConfig>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            push_to: Vec::new(),
        }
    }
}

impl Default for Gb28181Config {
    fn default() -> Self {
        Self {
            enabled: false,
            sip_port: default_gb28181_sip_port(),
            media_port: default_gb28181_media_port(),
            realm: default_gb28181_realm(),
            password: None,
            host: None,
        }
    }
}

fn default_srt_port() -> u16 {
    9000
}
fn default_gb28181_sip_port() -> u16 {
    5060
}
fn default_gb28181_media_port() -> u16 {
    10000
}
fn default_gb28181_realm() -> String {
    "zlmediakit".to_string()
}
fn default_srt_latency() -> u32 {
    120
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_srt_port(),
            latency_ms: default_srt_latency(),
            passphrase: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    #[serde(default)]
    pub app: String,
    #[serde(default = "default_record_path")]
    pub path: String,
    #[serde(default)]
    pub hls: bool,
    #[serde(default)]
    pub mp4: bool,
    #[serde(default)]
    pub flv: bool,
}

fn default_rtmp_port() -> u16 {
    1935
}
fn default_rtsp_port() -> u16 {
    8554
}
fn default_http_port() -> u16 {
    8080
}
fn default_api_port() -> u16 {
    8081
}
fn default_rtp_port() -> u16 {
    8000
}
fn default_webrtc_port() -> u16 {
    9000
}
fn default_vhost() -> String {
    "__defaultVhost__".to_string()
}
fn default_secret() -> String {
    "035c73f7-bb6b-4889-a715-d9eb2d1925cc".to_string()
}

fn default_auth_enabled() -> bool {
    false
}
fn default_flow_threshold() -> i64 {
    1024
}
fn default_stream_none_reader_delay() -> u64 {
    10
}
fn default_true() -> bool {
    true
}

fn default_proxy_app() -> String {
    "live".to_string()
}

fn default_record_path() -> String {
    "./record".to_string()
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            flow_threshold: default_flow_threshold(),
            stream_none_reader_delay: default_stream_none_reader_delay(),
        }
    }
}

impl Default for RtmpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            ssl: false,
            ssl_cert: None,
            ssl_key: None,
            ssl_port: None,
        }
    }
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            ssl: false,
            ssl_cert: None,
            ssl_key: None,
            ssl_port: None,
            tcp_mode: true,
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            ssl: false,
            ssl_cert: None,
            ssl_key: None,
            ssl_port: None,
            dir_root: true,
            www_root: default_www_root(),
        }
    }
}

fn default_www_root() -> String {
    "./www".to_string()
}

impl Default for ProxyEntryConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            vhost: default_vhost(),
            app: default_proxy_app(),
            stream: String::new(),
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pulls: vec![],
        }
    }
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            app: "record".to_string(),
            path: default_record_path(),
            hls: false,
            mp4: false,
            flv: false,
        }
    }
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            ice_servers: vec![],
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            rtmp_port: default_rtmp_port(),
            rtsp_port: default_rtsp_port(),
            http_port: default_http_port(),
            api_port: default_api_port(),
            rtp_port: default_rtp_port(),
            default_vhost: default_vhost(),
            secret: default_secret(),
            auth_enabled: default_auth_enabled(),
            hook: HookConfig::default(),
            general: GeneralConfig::default(),
            rtmp: RtmpConfig::default(),
            rtsp: RtspConfig::default(),
            http: HttpConfig::default(),
            record: RecordConfig::default(),
            proxy: ProxyConfig::default(),
            srt: SrtConfig::default(),
            gb28181: Gb28181Config::default(),
            cluster: ClusterConfig::default(),
            webrtc: WebRtcConfig::default(),
            webrtc_port: default_webrtc_port(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_defaults() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.rtmp_port, 1935);
        assert_eq!(cfg.rtsp_port, 8554);
        assert_eq!(cfg.http_port, 8080);
        assert_eq!(cfg.api_port, 8081);
        assert_eq!(cfg.default_vhost, "__defaultVhost__");
        assert!(cfg.rtmp.enabled);
        assert!(cfg.rtsp.enabled);
        assert!(cfg.http.enabled);
        assert!(cfg.http.dir_root);
    }

    #[test]
    fn rtmp_config_default() {
        let cfg = RtmpConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, None);
        assert!(!cfg.ssl);
        assert!(cfg.ssl_cert.is_none());
        assert!(cfg.ssl_key.is_none());
        assert!(cfg.ssl_port.is_none());
    }

    #[test]
    fn rtsp_config_default() {
        let cfg = RtspConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.tcp_mode);
        assert!(!cfg.ssl);
    }

    #[test]
    fn http_config_default() {
        let cfg = HttpConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.dir_root);
        assert_eq!(cfg.www_root, "./www");
        assert!(!cfg.ssl);
    }

    #[test]
    fn record_config_default() {
        let cfg = RecordConfig::default();
        assert_eq!(cfg.app, "record");
        assert!(!cfg.hls);
        assert!(!cfg.flv);
        assert!(!cfg.mp4);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = ServerConfig::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: ServerConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.rtmp_port, cfg.rtmp_port);
        assert_eq!(parsed.rtsp_port, cfg.rtsp_port);
        assert_eq!(parsed.http_port, cfg.http_port);
        assert_eq!(parsed.api_port, cfg.api_port);
        assert_eq!(parsed.default_vhost, cfg.default_vhost);
        assert_eq!(parsed.rtmp.enabled, cfg.rtmp.enabled);
        assert_eq!(parsed.http.dir_root, cfg.http.dir_root);
    }

    #[test]
    fn toml_with_overrides() {
        let toml_str = r#"
rtmp_port = 19350
rtsp_port = 8554
http_port = 8080
api_port = 9090

[rtmp]
enabled = false
port = 1935

[http]
enabled = true
dir_root = false
"#;
        let cfg: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.rtmp_port, 19350);
        assert_eq!(cfg.rtsp_port, 8554);
        assert_eq!(cfg.http_port, 8080);
        assert!(!cfg.rtmp.enabled);
        assert!(!cfg.http.dir_root);
    }

    #[test]
    fn webrtc_config_default() {
        let cfg = WebRtcConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.ice_servers.is_empty());
        assert!(cfg.port.is_none());
    }

    #[test]
    fn general_config_default() {
        let cfg = GeneralConfig::default();
        assert!(cfg.flow_threshold > 0);
        assert!(cfg.stream_none_reader_delay > 0);
    }

    #[test]
    fn hook_config_default() {
        let cfg = HookConfig::default();
        assert!(cfg.on_publish.is_none());
        assert!(cfg.on_play.is_none());
        assert!(cfg.on_stream_not_found.is_none());
        assert_eq!(cfg.timeout_sec, 5);
        assert_eq!(cfg.retry, 1);
    }
}

impl ServerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}
