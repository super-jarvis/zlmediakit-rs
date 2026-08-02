use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Runtime server configuration store, addressable by dotted keys
/// (`api.secret`, `rtmp.port`, ...) exactly like the original ZLMediaKit.
/// Seeded from `ServerConfig::default()`; `setServerConfig` mutates it at
/// runtime and `getServerConfig` reports its current snapshot.
static RUNTIME_CONFIG: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(runtime_config_values(&ServerConfig::default())));

fn runtime_config_values(d: &ServerConfig) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("api.apiDebug".to_string(), "0".to_string());
    m.insert("api.secret".to_string(), d.secret.clone());
    m.insert(
        "general.mediaServerId".to_string(),
        "zlmediakit-rs".to_string(),
    );
    m.insert("general.listenIP".to_string(), d.listen_ip.clone());
    m.insert(
        "general.flowThreshold".to_string(),
        d.general.flow_threshold.to_string(),
    );
    m.insert(
        "general.streamNoneReaderDelayMS".to_string(),
        d.general.stream_none_reader_delay.to_string(),
    );
    m.insert("rtmp.port".to_string(), d.rtmp_port.to_string());
    m.insert("rtmp.enabled".to_string(), bool_to_str(d.rtmp.enabled));
    m.insert("rtmp.ssl".to_string(), bool_to_str(d.rtmp.ssl));
    m.insert("rtmp.ssl_cert".to_string(), option_to_str(&d.rtmp.ssl_cert));
    m.insert("rtmp.ssl_key".to_string(), option_to_str(&d.rtmp.ssl_key));
    m.insert(
        "rtmp.ssl_port".to_string(),
        option_display(&d.rtmp.ssl_port),
    );
    m.insert("rtsp.port".to_string(), d.rtsp_port.to_string());
    m.insert("rtsp.enabled".to_string(), bool_to_str(d.rtsp.enabled));
    m.insert("rtsp.ssl".to_string(), bool_to_str(d.rtsp.ssl));
    m.insert("rtsp.ssl_cert".to_string(), option_to_str(&d.rtsp.ssl_cert));
    m.insert("rtsp.ssl_key".to_string(), option_to_str(&d.rtsp.ssl_key));
    m.insert(
        "rtsp.ssl_port".to_string(),
        option_display(&d.rtsp.ssl_port),
    );
    m.insert("http.port".to_string(), d.http_port.to_string());
    m.insert("http.enabled".to_string(), bool_to_str(d.http.enabled));
    m.insert("http.ssl".to_string(), bool_to_str(d.http.ssl));
    m.insert("http.ssl_cert".to_string(), option_to_str(&d.http.ssl_cert));
    m.insert("http.ssl_key".to_string(), option_to_str(&d.http.ssl_key));
    m.insert(
        "http.ssl_port".to_string(),
        option_display(&d.http.ssl_port),
    );
    m.insert("api.port".to_string(), d.api_port.to_string());
    m.insert("record.path".to_string(), d.record.path.clone());
    m.insert("record.app".to_string(), d.record.app.clone());
    m.insert("record.hls".to_string(), bool_to_str(d.record.hls));
    m.insert("record.mp4".to_string(), bool_to_str(d.record.mp4));
    m.insert("record.flv".to_string(), bool_to_str(d.record.flv));
    m.insert("srt.port".to_string(), d.srt.port.to_string());
    m.insert("srt.enabled".to_string(), bool_to_str(d.srt.enabled));
    m.insert(
        "gb28181.sip_port".to_string(),
        d.gb28181.sip_port.to_string(),
    );
    m.insert(
        "gb28181.media_port".to_string(),
        d.gb28181.media_port.to_string(),
    );
    m.insert(
        "gb28181.enabled".to_string(),
        bool_to_str(d.gb28181.enabled),
    );
    m.insert("webrtc.port".to_string(), d.webrtc_port.to_string());
    m.insert("webrtc.enabled".to_string(), bool_to_str(d.webrtc.enabled));
    m.insert(
        "hook.on_publish".to_string(),
        option_to_str(&d.hook.on_publish),
    );
    m.insert("hook.on_play".to_string(), option_to_str(&d.hook.on_play));
    m.insert(
        "hook.on_stream_not_found".to_string(),
        option_to_str(&d.hook.on_stream_not_found),
    );
    m.insert(
        "hook.on_stream_none_reader".to_string(),
        option_to_str(&d.hook.on_stream_none_reader),
    );
    m.insert(
        "hook.on_stream_changed".to_string(),
        option_to_str(&d.hook.on_stream_changed),
    );
    m.insert(
        "hook.on_record_mp4".to_string(),
        option_to_str(&d.hook.on_record_mp4),
    );
    m.insert(
        "hook.on_rtsp_realm".to_string(),
        option_to_str(&d.hook.on_rtsp_realm),
    );
    m.insert(
        "hook.on_server_started".to_string(),
        option_to_str(&d.hook.on_server_started),
    );
    m.insert(
        "hook.on_server_exited".to_string(),
        option_to_str(&d.hook.on_server_exited),
    );
    m.insert(
        "hook.on_http_access".to_string(),
        option_to_str(&d.hook.on_http_access),
    );
    m.insert(
        "hook.on_flow_report".to_string(),
        option_to_str(&d.hook.on_flow_report),
    );
    m.insert(
        "hook.flow_report_interval_sec".to_string(),
        d.hook.flow_report_interval_sec.to_string(),
    );
    m.insert(
        "hook.timeout_sec".to_string(),
        d.hook.timeout_sec.to_string(),
    );
    m.insert("hook.retry".to_string(), d.hook.retry.to_string());
    m
}

fn option_to_str(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

fn option_display<T: std::fmt::Display>(value: &Option<T>) -> String {
    value.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn bool_to_str(v: bool) -> String {
    if v {
        "1".to_string()
    } else {
        "0".to_string()
    }
}

/// Snapshot of the current runtime config (dotted keys -> string values).
pub fn runtime_config_snapshot() -> HashMap<String, String> {
    RUNTIME_CONFIG.read().unwrap().clone()
}

pub fn runtime_config_value(key: &str) -> Option<String> {
    RUNTIME_CONFIG.read().unwrap().get(key).cloned()
}

/// Replaces the runtime snapshot with the effective startup configuration.
/// Call this after config-file and command-line overrides have been applied.
pub fn sync_runtime_config(config: &ServerConfig) {
    *RUNTIME_CONFIG.write().unwrap() = runtime_config_values(config);
}

/// Sets a runtime config key, returning `true` if the value changed.
pub fn set_runtime_config(key: &str, value: &str) -> bool {
    let mut g = RUNTIME_CONFIG.write().unwrap();
    if !g.contains_key(key) {
        return false;
    }
    let changed = g.get(key).map(|v| v.as_str()) != Some(value);
    if changed {
        g.insert(key.to_string(), value.to_string());
    }
    changed
}

pub fn has_runtime_config_key(key: &str) -> bool {
    RUNTIME_CONFIG.read().unwrap().contains_key(key)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// IP address used by protocol signalling/listening sockets. Use `::` for
    /// IPv6 (and dual-stack where the operating system enables it).
    #[serde(default = "default_listen_ip")]
    pub listen_ip: String,

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

    /// RTSP Digest user table (`username = "password"`). When set, Digest auth
    /// validates each user against their own password; unknown users fall back
    /// to `secret`. Example:
    /// ```toml
    /// [auth.users]
    /// admin = "admin123"
    /// ```
    #[serde(default)]
    pub auth_users: HashMap<String, String>,

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

    /// Stable identifier reported to hooks (e.g. `on_server_started`,
    /// `on_flow_report`). Defaults to the machine hostname when empty.
    #[serde(default = "default_media_server_id")]
    pub media_server_id: String,
}

fn default_media_server_id() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "zlmediakit-rs".to_string())
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
    /// Static RTSP Digest authentication realm used as a fallback when no
    /// `hook.on_rtsp_realm` is configured (or it returns nothing). Empty means
    /// no static realm — a Digest challenge is only issued when a realm is
    /// available from the hook. Defaults to `"zlmediakit"` to mirror upstream
    /// behavior so Digest auth works out-of-the-box.
    #[serde(default = "default_rtsp_realm")]
    pub realm: String,
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
    /// UDP port shared by every ICE session. When omitted, the WebRTC HTTP
    /// signalling port is reused (TCP and UDP can use the same port number).
    #[serde(default)]
    pub ice_port: Option<u16>,
    /// Local IP used by the shared ICE UDP socket. Use `::` for IPv6/dual-stack
    /// deployments when supported by the operating system.
    #[serde(default = "default_webrtc_ice_bind_ip")]
    pub ice_bind_ip: String,
    /// Run as an ICE-lite server. Full ICE remains the safer default when the
    /// peer or NAT topology is not known in advance.
    #[serde(default)]
    pub ice_lite: bool,
    /// Public addresses advertised as host candidates for 1:1 NAT deployments.
    #[serde(default)]
    pub external_ips: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

    /// Called when a published stream has no more readers (all players gone).
    /// POST params: vhost, app, stream, regist, totalReaderCount
    #[serde(default)]
    pub on_stream_none_reader: Option<String>,

    /// Called when a stream's reader count changes (a player joins/leaves).
    /// POST params: vhost, app, stream, regist, totalReaderCount
    #[serde(default)]
    pub on_stream_changed: Option<String>,

    /// Called after an MP4 recording file is finalized on disk.
    /// POST params: vhost, app, stream, period, file_path, file_size, url,
    /// time_len, start_time, time_stamp
    #[serde(default)]
    pub on_record_mp4: Option<String>,

    /// Called to resolve the RTSP authentication realm for a stream.
    /// POST params: vhost, app, stream, ip
    /// Response should include `realm` to use; absence falls back to default.
    #[serde(default)]
    pub on_rtsp_realm: Option<String>,

    /// Called once after the media server has finished starting up.
    /// POST params: mediaServerId
    #[serde(default)]
    pub on_server_started: Option<String>,

    /// Called once just before the media server process exits.
    /// POST params: mediaServerId
    #[serde(default)]
    pub on_server_exited: Option<String>,

    /// Per-request HTTP access control hook. Called before serving any HTTP
    /// request (including HTTP-FLV / HLS playback and static files).
    /// POST params: vhost, app, stream, ip, params, path, is_play, url
    /// Returning `{code:1}` denies the request; `{code:0}` allows it.
    #[serde(default)]
    pub on_http_access: Option<String>,

    /// Periodic traffic/bandwidth report hook. Called every
    /// `flow_report_interval_sec` seconds. POST params: mediaServerId, bytes_in,
    /// bytes_out, total_readable, total_writable
    #[serde(default)]
    pub on_flow_report: Option<String>,

    /// Interval in seconds between `on_flow_report` callbacks (default 30).
    #[serde(default = "default_flow_interval")]
    pub flow_report_interval_sec: u64,

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
fn default_flow_interval() -> u64 {
    30
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            on_publish: None,
            on_play: None,
            on_stream_not_found: None,
            on_stream_none_reader: None,
            on_stream_changed: None,
            on_record_mp4: None,
            on_rtsp_realm: None,
            on_server_started: None,
            on_server_exited: None,
            on_http_access: None,
            on_flow_report: None,
            flow_report_interval_sec: default_flow_interval(),
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
    /// Origin server base URL used for edge auto-pull (origin redirection).
    /// When set, a play request for a stream that is not currently available
    /// locally causes the edge node to automatically pull it from this origin
    /// (e.g. `rtmp://origin:1935` or `rtsp://origin:554`). Leave empty to
    /// disable edge pull.
    #[serde(default)]
    pub origin_url: Option<String>,
    /// Ordered origin candidates. Each item may be a base URL or a template
    /// containing `{vhost}`, `{app}` and `{stream}`. `origin_url` remains as a
    /// backwards-compatible first candidate.
    #[serde(default)]
    pub origin_urls: Vec<String>,
    /// Maximum time to wait for each origin candidate to publish media.
    #[serde(default = "default_origin_timeout_ms")]
    pub origin_timeout_ms: u64,
    /// Upstream vhost substituted into an origin URL template.
    #[serde(default = "default_vhost")]
    pub origin_vhost: String,
    /// Upstream app appended to base URLs or substituted into templates.
    #[serde(default = "default_proxy_app")]
    pub origin_app: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            push_to: Vec::new(),
            origin_url: None,
            origin_urls: Vec::new(),
            origin_timeout_ms: default_origin_timeout_ms(),
            origin_vhost: default_vhost(),
            origin_app: default_proxy_app(),
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
fn default_listen_ip() -> String {
    "0.0.0.0".to_string()
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
fn default_webrtc_ice_bind_ip() -> String {
    "0.0.0.0".to_string()
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
fn default_origin_timeout_ms() -> u64 {
    5_000
}
fn default_true() -> bool {
    true
}
fn default_rtsp_realm() -> String {
    "zlmediakit".to_string()
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
            realm: default_rtsp_realm(),
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
            ice_port: None,
            ice_bind_ip: default_webrtc_ice_bind_ip(),
            ice_lite: false,
            external_ips: vec![],
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_ip: default_listen_ip(),
            rtmp_port: default_rtmp_port(),
            rtsp_port: default_rtsp_port(),
            http_port: default_http_port(),
            api_port: default_api_port(),
            rtp_port: default_rtp_port(),
            default_vhost: default_vhost(),
            secret: default_secret(),
            auth_enabled: default_auth_enabled(),
            auth_users: HashMap::new(),
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
            media_server_id: default_media_server_id(),
        }
    }
}

impl ServerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_defaults() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.listen_ip, "0.0.0.0");
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
        assert_eq!(parsed.listen_ip, cfg.listen_ip);
        assert_eq!(parsed.default_vhost, cfg.default_vhost);
        assert_eq!(parsed.rtmp.enabled, cfg.rtmp.enabled);
        assert_eq!(parsed.http.dir_root, cfg.http.dir_root);
    }

    #[test]
    fn toml_with_overrides() {
        let toml_str = r#"
listen_ip = "::"
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
        assert_eq!(cfg.listen_ip, "::");
        assert!(!cfg.rtmp.enabled);
        assert!(!cfg.http.dir_root);
    }

    #[test]
    fn webrtc_config_default() {
        let cfg = WebRtcConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.ice_servers.is_empty());
        assert!(cfg.port.is_none());
        assert!(cfg.ice_port.is_none());
        assert_eq!(cfg.ice_bind_ip, "0.0.0.0");
        assert!(!cfg.ice_lite);
        assert!(cfg.external_ips.is_empty());
    }

    #[test]
    fn runtime_config_rejects_unknown_keys() {
        assert!(!has_runtime_config_key("unknown.section.key"));
        assert!(!set_runtime_config("unknown.section.key", "value"));
        assert!(runtime_config_value("unknown.section.key").is_none());
    }

    #[test]
    fn runtime_config_contains_all_dynamic_hook_keys() {
        let mut cfg = ServerConfig::default();
        cfg.hook.on_play = Some("https://hooks.example.test/on_play".to_string());
        cfg.hook.flow_report_interval_sec = 9;
        let values = runtime_config_values(&cfg);
        assert_eq!(
            values.get("hook.on_play").map(String::as_str),
            Some("https://hooks.example.test/on_play")
        );
        assert_eq!(
            values
                .get("hook.flow_report_interval_sec")
                .map(String::as_str),
            Some("9")
        );
        assert_eq!(
            values.get("hook.timeout_sec").map(String::as_str),
            Some("5")
        );
        assert_eq!(values.get("hook.retry").map(String::as_str), Some("1"));
        assert_eq!(values.get("hook.on_publish").map(String::as_str), Some(""));
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
        assert!(cfg.on_stream_none_reader.is_none());
        assert!(cfg.on_stream_changed.is_none());
        assert!(cfg.on_record_mp4.is_none());
        assert!(cfg.on_rtsp_realm.is_none());
        assert_eq!(cfg.timeout_sec, 5);
        assert_eq!(cfg.retry, 1);
    }
}
