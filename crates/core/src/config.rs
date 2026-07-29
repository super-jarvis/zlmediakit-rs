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
    #[serde(default = "default_true")]
    pub ssl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_true")]
    pub ssl: bool,
    #[serde(default = "default_true")]
    pub tcp_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_true")]
    pub ssl: bool,
    #[serde(default = "default_true")]
    pub dir_root: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRtcConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub port: Option<u16>,
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
        }
    }
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            ssl: false,
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
            dir_root: true,
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
            general: GeneralConfig::default(),
            rtmp: RtmpConfig::default(),
            rtsp: RtspConfig::default(),
            http: HttpConfig::default(),
            record: RecordConfig::default(),
            webrtc: WebRtcConfig::default(),
            webrtc_port: default_webrtc_port(),
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
