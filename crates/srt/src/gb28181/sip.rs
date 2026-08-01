//! GB28181 SIP server (UDP).
//!
//! Implements the subset of SIP needed to act as a GB28181 platform:
//! * `REGISTER` handling with optional Digest authentication, registration
//!   expiry (`Expires` header) and explicit un-registration (`Expires: 0`).
//! * `MESSAGE` handling for GB28181 XML control commands: `Keepalive`,
//!   `Catalog` and `DeviceInfo` (both unsolicited pushes and responses to our
//!   queries). Device online/offline state is tracked and expiring devices are
//!   cleaned up automatically.
//! * Platform-initiated `INVITE` (`query_catalog`/`query_device_info` and
//!   `invite`) to request a stream; the device is told (via the SDP `c=`/`m=`
//!   lines) to send RTP/PS to a local media port which is opened on the fly via
//!   [`RtpServerManager`].
//! * Receiving an `INVITE` from a device (push mode) and `BYE` teardown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use zlmediakit_core::MediaSourceManager;

use super::rtp_server::{RtpPayloadType, RtpServerManager};
use super::xml::{parse_xml, XmlNode};

/// Configuration for the SIP signaling server.
#[derive(Debug, Clone)]
pub struct SipServerConfig {
    pub sip_port: u16,
    pub realm: String,
    pub password: Option<String>,
    pub host: Option<String>,
    pub media_port_base: u16,
    pub vhost: String,
}

/// A channel (camera/encoder) reported by a device via a `Catalog` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub channel_id: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub owner: Option<String>,
    pub civil_code: Option<String>,
    pub block: Option<String>,
    pub address: Option<String>,
    pub parental: Option<u32>,
    pub parent_id: Option<String>,
    pub safety_way: Option<u32>,
    pub register_way: Option<u32>,
    pub cert_num: Option<String>,
    pub certifiable: Option<u32>,
    pub err_code: Option<u32>,
    pub end_time: Option<String>,
    pub secrecy: Option<u32>,
    pub ip_address: Option<String>,
    pub port: Option<u32>,
    pub password: Option<String>,
    pub ptz_type: Option<u32>,
    /// "ON" when the channel is online, "OFF" otherwise.
    pub status: String,
    pub longitude: Option<f64>,
    pub latitude: Option<f64>,
}

impl ChannelInfo {
    fn from_xml(item: &XmlNode) -> Option<Self> {
        let channel_id = item.text_of("DeviceID")?.trim().to_string();
        let num = |n: &str| item.text_of(n).and_then(|s| s.trim().parse().ok());
        let flt = |n: &str| item.text_of(n).and_then(|s| s.trim().parse().ok());
        Some(Self {
            channel_id,
            name: item.text_of("Name").unwrap_or("").trim().to_string(),
            manufacturer: item.text_of("Manufacturer").map(str::trim).map(str::to_string),
            model: item.text_of("Model").map(str::trim).map(str::to_string),
            owner: item.text_of("Owner").map(str::trim).map(str::to_string),
            civil_code: item.text_of("CivilCode").map(str::trim).map(str::to_string),
            block: item.text_of("Block").map(str::trim).map(str::to_string),
            address: item.text_of("Address").map(str::trim).map(str::to_string),
            parental: num("Parental"),
            parent_id: item.text_of("ParentID").map(str::trim).map(str::to_string),
            safety_way: num("SafetyWay"),
            register_way: num("RegisterWay"),
            cert_num: item.text_of("CertNum").map(str::trim).map(str::to_string),
            certifiable: num("Certifiable"),
            err_code: num("ErrCode"),
            end_time: item.text_of("EndTime").map(str::trim).map(str::to_string),
            secrecy: num("Secrecy"),
            ip_address: item.text_of("IPAddress").map(str::trim).map(str::to_string),
            port: num("Port"),
            password: item.text_of("Password").map(str::trim).map(str::to_string),
            ptz_type: num("PTZType"),
            status: item.text_of("Status").unwrap_or("OFF").trim().to_string(),
            longitude: flt("Longitude"),
            latitude: flt("Latitude"),
        })
    }
}

/// Static device metadata reported via a `DeviceInfo` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub address: Option<String>,
    pub civil_code: Option<String>,
    pub channel_num: Option<u32>,
    pub result: Option<String>,
}

impl DeviceInfo {
    fn from_xml(root: &XmlNode) -> Self {
        let num = |n: &str| root.text_of(n).and_then(|s| s.trim().parse().ok());
        Self {
            device_id: root.text_of("DeviceID").unwrap_or("").trim().to_string(),
            name: root.text_of("DeviceName").map(str::trim).map(str::to_string),
            manufacturer: root.text_of("Manufacturer").map(str::trim).map(str::to_string),
            model: root.text_of("Model").map(str::trim).map(str::to_string),
            firmware: root.text_of("Firmware").map(str::trim).map(str::to_string),
            address: root.text_of("Address").map(str::trim).map(str::to_string),
            civil_code: root.text_of("CivilCode").map(str::trim).map(str::to_string),
            channel_num: num("Channel"),
            result: root.text_of("Result").map(str::trim).map(str::to_string),
        }
    }
}

/// Snapshot of a registered device, returned by list/get APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListItem {
    pub device_id: String,
    pub online: bool,
    pub ip: String,
    pub port: u16,
    /// Milliseconds since the device was last heard from.
    pub last_seen_ms: u64,
    /// Milliseconds until the registration expires.
    pub expires_ms: u64,
    pub name: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub channels: Vec<ChannelInfo>,
}

/// Snapshot of the SIP signaling server itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipInfo {
    pub sip_port: u16,
    pub realm: String,
    pub host: String,
    pub device_count: usize,
    pub active_streams: usize,
    pub stopped: bool,
}

#[derive(Debug, Clone)]
struct DeviceContact {
    addr: SocketAddr,
    last_seen: Instant,
    expires: Duration,
    info: Option<DeviceInfo>,
    channels: Vec<ChannelInfo>,
}

#[derive(Debug, Clone)]
struct InviteCtx {
    device_id: String,
    channel_id: String,
    media_port: u16,
}

/// A parsed SIP message.
struct SipMessage {
    method: Option<String>,
    status: Option<u16>,
    uri: String,
    headers: HashMap<String, String>,
    body: String,
}

impl SipMessage {
    fn get(&self, name: &str) -> Option<&String> {
        self.headers.get(&name.to_ascii_lowercase())
    }
}

fn parse_sip(buf: &[u8]) -> Option<SipMessage> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.split("\r\n");
    let first = lines.next()?.to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut body = String::new();
    let mut in_body = false;
    let mut content_length = 0usize;
    for line in lines {
        if in_body {
            body.push_str(line);
            body.push('\n');
            continue;
        }
        if line.is_empty() {
            in_body = true;
            continue;
        }
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim().to_ascii_lowercase();
            let val = line[pos + 1..].trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }
    // Truncate body to content-length.
    if content_length > 0 && body.len() > content_length {
        body.truncate(content_length);
    }

    let mut parts = first.split_whitespace();
    let a = parts.next().unwrap_or("").to_string();
    let b = parts.next().unwrap_or("").to_string();
    let c = parts.next().unwrap_or("").to_string();

    let (method, status, uri) = if a.starts_with("SIP/2.0") {
        (None, c.parse::<u16>().ok(), b)
    } else {
        (Some(a), None, b)
    };

    Some(SipMessage {
        method,
        status,
        uri,
        headers,
        body,
    })
}

fn uri_user(hdr: &str) -> Option<String> {
    let s = hdr.trim().trim_start_matches('<');
    let s = if let Some(p) = s.find("sip:") {
        &s[p + 4..]
    } else {
        s
    };
    if let Some(at) = s.find('@') {
        Some(s[..at].to_string())
    } else if let Some(colon) = s.find(':') {
        Some(s[..colon].to_string())
    } else if let Some(semi) = s.find(';') {
        Some(s[..semi].to_string())
    } else if !s.is_empty() {
        Some(s.to_string())
    } else {
        None
    }
}

fn gen_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:016x}", nanos % 0xFFFF_FFFF_FFFF_FFFF)
}

fn gen_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos)
}

fn md5_hex(s: &str) -> String {
    let digest = md5::compute(s.as_bytes());
    let mut out = String::with_capacity(32);
    for b in digest.0 {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn sdp_value(body: &str, prefix: &str) -> Option<String> {
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Extract the channels listed in a `Catalog` response.
fn parse_catalog(root: &XmlNode) -> Vec<ChannelInfo> {
    let mut out = Vec::new();
    for item in root.descendants_named("Item") {
        if let Some(ch) = ChannelInfo::from_xml(item) {
            out.push(ch);
        }
    }
    out
}

/// The SIP server.
pub struct SipServer {
    socket: UdpSocket,
    config: SipServerConfig,
    rtp: Arc<RtpServerManager>,
    devices: DashMap<String, DeviceContact>,
    pending: DashMap<String, InviteCtx>,
    active: DashMap<String, InviteCtx>,
    pending_queries: DashMap<u32, oneshot::Sender<XmlNode>>,
    sn_counter: AtomicU32,
    shutdown: std::sync::atomic::AtomicBool,
}

impl SipServer {
    pub async fn new(
        config: SipServerConfig,
        manager: Arc<MediaSourceManager>,
        media_port_base: u16,
    ) -> anyhow::Result<Arc<Self>> {
        let socket = UdpSocket::bind(("0.0.0.0", config.sip_port)).await?;
        let rtp = RtpServerManager::new(manager, media_port_base);
        Ok(Arc::new(Self {
            socket,
            config,
            rtp,
            devices: DashMap::new(),
            pending: DashMap::new(),
            active: DashMap::new(),
            pending_queries: DashMap::new(),
            sn_counter: AtomicU32::new(0),
            shutdown: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub fn rtp_manager(&self) -> Arc<RtpServerManager> {
        self.rtp.clone()
    }

    /// The UDP port the SIP socket is bound to.
    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Request a graceful shutdown: the receive loop exits within one poll
    /// interval, all registered devices are dropped and active streams closed.
    pub fn stop(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let ids: Vec<String> = self.devices.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            self.unregister_device(&id, "sip stopped");
        }
        info!("gb28181 sip server stopped");
    }

    /// Whether [`Self::stop`] has been requested.
    pub fn is_stopped(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn host(&self) -> String {
        if let Some(h) = &self.config.host {
            if !h.is_empty() {
                return h.clone();
            }
        }
        local_ip()
    }

    fn next_sn(&self) -> u32 {
        self.sn_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn registered(&self, device_id: &str) -> anyhow::Result<DeviceContact> {
        self.devices
            .get(device_id)
            .map(|d| d.clone())
            .ok_or_else(|| anyhow::anyhow!("device {device_id} not registered"))
    }

    /// Request a stream from a registered device by channel id.
    /// Opens a local RTP media port and sends an `INVITE`. Returns the media port.
    pub async fn invite(&self, device_id: &str, channel_id: &str) -> anyhow::Result<u16> {
        let device = self.registered(device_id)?;
        let media_port = self.rtp_port_for_invite();
        self.rtp
            .open(
                media_port,
                &self.config.vhost,
                "gb28181",
                channel_id,
                RtpPayloadType::Ps,
                None,
            )
            .await?;

        let call_id = format!("{}@{}", gen_tag(), self.host());
        let branch = format!("z9hG4bK{}", gen_tag());
        let tag = gen_tag();
        let from = format!("<sip:{}@{}>", self.config.realm, self.host());
        let to = format!("<sip:{}@{}>", device_id, device.addr.ip());
        let host = self.host();
        let ssrc = gen_ssrc(channel_id);
        let sdp = format!(
            "v=0\r\n\
             o={} 0 0 IN IP4 {}\r\n\
             s=Play\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=video {} RTP/AVP 96\r\n\
             a=rtpmap:96 PS/90000\r\n\
             a=recvonly\r\n\
             y={:09}\r\n",
            self.config.realm, host, host, media_port, ssrc
        );
        let invite = format!(
            "INVITE sip:{}@{}:{} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {};branch={}\r\n\
             From: {};tag={}\r\n\
             To: {}\r\n\
             Call-ID: {}\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:{}@{}:{}>\r\n\
             Content-Type: application/sdp\r\n\
             Max-Forwards: 70\r\n\
             Subject: {}\r\n\
             Content-Length: {}\r\n\r\n{}",
            channel_id,
            device.addr.ip(),
            device.addr.port(),
            host,
            branch,
            from,
            tag,
            to,
            call_id,
            self.config.realm,
            host,
            self.config.sip_port,
            channel_id,
            sdp.len(),
            sdp
        );
        self.socket.send_to(invite.as_bytes(), device.addr).await?;
        info!(device = device_id, channel = channel_id, media_port, "gb28181 invite sent");
        self.pending.insert(
            call_id,
            InviteCtx {
                device_id: device_id.to_string(),
                channel_id: channel_id.to_string(),
                media_port,
            },
        );
        Ok(media_port)
    }

    /// Query the device's channel catalog. Returns the list of channels and
    /// caches it on the device record.
    pub async fn query_catalog(&self, device_id: &str) -> anyhow::Result<Vec<ChannelInfo>> {
        let device = self.registered(device_id)?;
        let sn = self.next_sn();
        let xml = format!(
            "<?xml version=\"1.0\"?>\n<Query>\n<CmdType>Catalog</CmdType>\n<SN>{sn}</SN>\n<DeviceID>{device_id}</DeviceID>\n</Query>"
        );
        let root = self
            .send_query(device_id, &device, &xml, sn)
            .await?;
        let channels = parse_catalog(&root);
        if let Some(mut d) = self.devices.get_mut(device_id) {
            d.channels = channels.clone();
        }
        Ok(channels)
    }

    /// Query the device for its static information.
    pub async fn query_device_info(&self, device_id: &str) -> anyhow::Result<DeviceInfo> {
        let device = self.registered(device_id)?;
        let sn = self.next_sn();
        let xml = format!(
            "<?xml version=\"1.0\"?>\n<Query>\n<CmdType>DeviceInfo</CmdType>\n<SN>{sn}</SN>\n<DeviceID>{device_id}</DeviceID>\n</Query>"
        );
        let root = self.send_query(device_id, &device, &xml, sn).await?;
        let info = DeviceInfo::from_xml(&root);
        if let Some(mut d) = self.devices.get_mut(device_id) {
            d.info = Some(info.clone());
        }
        Ok(info)
    }

    /// Send an XML `MESSAGE` and wait for the matching response (by SN).
    async fn send_query(
        &self,
        device_id: &str,
        device: &DeviceContact,
        body: &str,
        sn: u32,
    ) -> anyhow::Result<XmlNode> {
        let (tx, rx) = oneshot::channel();
        self.pending_queries.insert(sn, tx);
        let host = self.host();
        let call_id = format!("{}@{}", gen_tag(), host);
        let branch = format!("z9hG4bK{}", gen_tag());
        let tag = gen_tag();
        let from = format!("<sip:{}@{}>", self.config.realm, host);
        let to = format!("<sip:{}@{}>", device_id, device.addr.ip());
        let msg = format!(
            "MESSAGE sip:{}@{}:{} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {};branch={}\r\n\
             From: {};tag={}\r\n\
             To: {}\r\n\
             Call-ID: {}\r\n\
             CSeq: 20 MESSAGE\r\n\
             Contact: <sip:{}@{}:{}>\r\n\
             Content-Type: Application/MANSCDP+xml\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: {}\r\n\r\n{}",
            device_id,
            device.addr.ip(),
            device.addr.port(),
            host,
            branch,
            from,
            tag,
            to,
            call_id,
            self.config.realm,
            host,
            self.config.sip_port,
            body.len(),
            body
        );
        if let Err(e) = self.socket.send_to(msg.as_bytes(), device.addr).await {
            self.pending_queries.remove(&sn);
            return Err(e.into());
        }
        let result = tokio::time::timeout(Duration::from_secs(5), rx).await;
        self.pending_queries.remove(&sn);
        match result {
            Ok(Ok(root)) => Ok(root),
            Ok(Err(_)) => anyhow::bail!("query {sn} response channel closed"),
            Err(_) => anyhow::bail!("query {sn} timed out"),
        }
    }

    fn rtp_port_for_invite(&self) -> u16 {
        // Allocate a fresh port via the manager's open(0) path by reserving.
        // We pick an incrementing port above the base to avoid collisions.
        use std::sync::atomic::AtomicU16;
        static NEXT: AtomicU16 = AtomicU16::new(0);
        let base = self.config.media_port_base;
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        base.saturating_add(n)
    }

    fn send_response(&self, addr: SocketAddr, status_line: &str, headers: &[(&str, String)], body: &str) {
        let mut msg = format!("SIP/2.0 {}\r\n", status_line);
        for (k, v) in headers {
            msg.push_str(&format!("{}: {}\r\n", k, v));
        }
        msg.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        msg.push_str(body);
        if let Err(e) = self.socket.try_send_to(msg.as_bytes(), addr) {
            warn!(%addr, "sip response send error: {e}");
        }
    }

    fn ack(&self, addr: SocketAddr, ctx: &InviteCtx, call_id: &str, to: &str, from: &str, via: &str, cseq: &str) {
        let ack = format!(
            "ACK sip:{}@{}:{} SIP/2.0\r\n\
             Via: {}\r\n\
             From: {}\r\n\
             To: {}\r\n\
             Call-ID: {}\r\n\
             CSeq: {} ACK\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n",
            ctx.channel_id,
            addr.ip(),
            addr.port(),
            via,
            from,
            to,
            call_id,
            cseq
        );
        let _ = self.socket.try_send_to(ack.as_bytes(), addr);
        self.active.insert(call_id.to_string(), ctx.clone());
        info!(channel = ctx.channel_id, media_port = ctx.media_port, "gb28181 stream active");
    }

    fn handle_register(&self, addr: SocketAddr, msg: &SipMessage) {
        let from = msg.get("from").cloned().unwrap_or_default();
        let device_id = match uri_user(&from) {
            Some(d) => d,
            None => return,
        };
        let to = msg.get("to").cloned().unwrap_or_default();
        let call_id = msg.get("call-id").cloned().unwrap_or_default();
        let cseq = msg.get("cseq").cloned().unwrap_or_default();
        let via = msg.get("via").cloned().unwrap_or_default();
        let contact = msg.get("contact").cloned().unwrap_or_default();
        let tag = gen_tag();

        // Digest authentication.
        if let Some(password) = &self.config.password {
            if !password.is_empty() {
                let auth = msg.get("authorization").cloned().unwrap_or_default();
                if !check_digest(
                    &auth,
                    &device_id,
                    password,
                    &msg.uri,
                    "REGISTER",
                    &self.config.realm,
                ) {
                    let nonce = gen_nonce();
                    let www = format!(
                        "Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5",
                        self.config.realm, nonce
                    );
                    self.send_response(
                        addr,
                        "401 Unauthorized",
                        &[
                            ("Via", via.clone()),
                            ("From", from.clone()),
                            ("To", format!("{};tag={}", to, tag)),
                            ("Call-ID", call_id.clone()),
                            ("CSeq", cseq.clone()),
                            ("Contact", contact.clone()),
                            ("WWW-Authenticate", www),
                        ],
                        "",
                    );
                    return;
                }
            }
        }

        let expires_secs = msg
            .get("expires")
            .and_then(|e| e.trim().parse::<u64>().ok())
            .unwrap_or(3600);
        if expires_secs == 0 {
            // Explicit un-registration.
            self.unregister_device(&device_id, "unregister");
        } else {
            self.devices.insert(
                device_id.clone(),
                DeviceContact {
                    addr,
                    last_seen: Instant::now(),
                    expires: Duration::from_secs(expires_secs),
                    info: None,
                    channels: Vec::new(),
                },
            );
            info!(device = device_id, %addr, expires = expires_secs, "gb28181 device registered");
        }
        self.send_response(
            addr,
            "200 OK",
            &[
                ("Via", via),
                ("From", from),
                ("To", format!("{};tag={}", to, tag)),
                ("Call-ID", call_id),
                ("CSeq", cseq),
                ("Contact", contact),
                ("Expires", expires_secs.to_string()),
            ],
            "",
        );
    }

    /// Deliver a response to a pending query if the SN matches.
    fn forward_query_response(&self, root: &XmlNode) {
        if let Some(sn) = root.text_of("SN").and_then(|s| s.trim().parse::<u32>().ok()) {
            if let Some((_, tx)) = self.pending_queries.remove(&sn) {
                let _ = tx.send(root.clone());
            }
        }
    }

    fn handle_message(&self, addr: SocketAddr, msg: &SipMessage) {
        let from = msg.get("from").cloned().unwrap_or_default();
        let device_id = uri_user(&from).unwrap_or_default();
        let to = msg.get("to").cloned().unwrap_or_default();
        let call_id = msg.get("call-id").cloned().unwrap_or_default();
        let cseq = msg.get("cseq").cloned().unwrap_or_default();
        let via = msg.get("via").cloned().unwrap_or_default();
        let contact = msg.get("contact").cloned().unwrap_or_default();
        let tag = gen_tag();

        // Track the device; a MESSAGE also serves as a keep-alive.
        if let Some(mut d) = self.devices.get_mut(&device_id) {
            d.last_seen = Instant::now();
        } else {
            self.devices.insert(
                device_id.clone(),
                DeviceContact {
                    addr,
                    last_seen: Instant::now(),
                    expires: Duration::from_secs(3600),
                    info: None,
                    channels: Vec::new(),
                },
            );
        }

        if let Some(root) = parse_xml(&msg.body) {
            match root.text_of("CmdType") {
                Some("Keepalive") => {
                    if let Some(interval) = root
                        .text_of("Interval")
                        .and_then(|s| s.trim().parse::<u64>().ok())
                    {
                        let expires = Duration::from_secs((interval * 3).max(60));
                        if let Some(mut d) = self.devices.get_mut(&device_id) {
                            d.expires = expires;
                        }
                    }
                    info!(device = device_id, "gb28181 device keepalive");
                }
                Some("Catalog") => {
                    let channels = parse_catalog(&root);
                    if let Some(mut d) = self.devices.get_mut(&device_id) {
                        d.channels = channels;
                    }
                    self.forward_query_response(&root);
                }
                Some("DeviceInfo") => {
                    let info = DeviceInfo::from_xml(&root);
                    if let Some(mut d) = self.devices.get_mut(&device_id) {
                        d.info = Some(info);
                    }
                    self.forward_query_response(&root);
                }
                Some(cmd) => debug!(device = device_id, cmd, "gb28181 message"),
                None => debug!(device = device_id, "gb28181 message without CmdType"),
            }
        } else {
            debug!(device = device_id, "gb28181 message with non-xml body");
        }

        self.send_response(
            addr,
            "200 OK",
            &[
                ("Via", via),
                ("From", from),
                ("To", format!("{};tag={}", to, tag)),
                ("Call-ID", call_id),
                ("CSeq", cseq),
                ("Contact", contact),
            ],
            "",
        );
    }

    async fn handle_invite_received(&self, addr: SocketAddr, msg: &SipMessage) {
        let to = msg.get("to").cloned().unwrap_or_default();
        let from = msg.get("from").cloned().unwrap_or_default();
        let call_id = msg.get("call-id").cloned().unwrap_or_default();
        let cseq = msg.get("cseq").cloned().unwrap_or_default();
        let via = msg.get("via").cloned().unwrap_or_default();
        let contact = msg.get("contact").cloned().unwrap_or_default();
        let tag = gen_tag();
        let device_id = uri_user(&from).unwrap_or_default();

        let y = sdp_value(&msg.body, "y=").unwrap_or_else(|| "0".to_string());
        let media_port = self.rtp_port_for_invite();
        let stream = format!("rtp_{}", y);
        if let Err(e) = self
            .rtp
            .open(
                media_port,
                &self.config.vhost,
                "rtp",
                &stream,
                RtpPayloadType::Ps,
                None,
            )
            .await
        {
            error!(%addr, "failed to open rtp server for push invite: {e}");
        } else {
            self.active.insert(
                call_id.clone(),
                InviteCtx {
                    device_id,
                    channel_id: stream.clone(),
                    media_port,
                },
            );
        }

        let host = self.host();
        let sdp = format!(
            "v=0\r\n\
             o={} 0 0 IN IP4 {}\r\n\
             s=Play\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=video {} RTP/AVP 96\r\n\
             a=rtpmap:96 PS/90000\r\n\
             a=sendonly\r\n\
             y={}\r\n",
            self.config.realm, host, host, media_port, y
        );
        self.send_response(
            addr,
            "200 OK",
            &[
                ("Via", via),
                ("From", from),
                ("To", format!("{};tag={}", to, tag)),
                ("Call-ID", call_id),
                ("CSeq", cseq),
                ("Contact", contact),
                ("Content-Type", "application/sdp".to_string()),
            ],
            &sdp,
        );
    }

    fn handle_bye(&self, addr: SocketAddr, msg: &SipMessage) {
        let to = msg.get("to").cloned().unwrap_or_default();
        let from = msg.get("from").cloned().unwrap_or_default();
        let call_id = msg.get("call-id").cloned().unwrap_or_default();
        let cseq = msg.get("cseq").cloned().unwrap_or_default();
        let via = msg.get("via").cloned().unwrap_or_default();
        let tag = gen_tag();
        if let Some((_, ctx)) = self.active.remove(&call_id) {
            self.rtp.close(ctx.media_port);
            info!(channel = ctx.channel_id, media_port = ctx.media_port, "gb28181 stream stopped");
        } else if let Some((_, ctx)) = self.pending.remove(&call_id) {
            self.rtp.close(ctx.media_port);
        }
        self.send_response(
            addr,
            "200 OK",
            &[
                ("Via", via),
                ("From", from),
                ("To", format!("{};tag={}", to, tag)),
                ("Call-ID", call_id),
                ("CSeq", cseq),
            ],
            "",
        );
    }

    fn handle_response(&self, addr: SocketAddr, msg: &SipMessage) {
        let call_id = match msg.get("call-id") {
            Some(c) => c.clone(),
            None => return,
        };
        let status = msg.status.unwrap_or(0);
        if status >= 200 {
            if let Some(ctx) = self.pending.remove(&call_id).map(|(_, c)| c) {
                if status == 200 {
                    let to = msg.get("to").cloned().unwrap_or_default();
                    let from = msg.get("from").cloned().unwrap_or_default();
                    let via = msg.get("via").cloned().unwrap_or_default();
                    let cseq = msg.get("cseq").cloned().unwrap_or_default();
                    self.ack(addr, &ctx, &call_id, &to, &from, &via, &cseq);
                } else {
                    self.rtp.close(ctx.media_port);
                    warn!(channel = ctx.channel_id, status, "gb28181 invite rejected");
                }
            }
        }
    }

    /// Remove a device (on un-registration or expiry) and close its streams.
    fn unregister_device(&self, device_id: &str, reason: &str) {
        if self.devices.remove(device_id).is_some() {
            let to_close: Vec<String> = self
                .active
                .iter()
                .filter(|e| e.value().device_id == device_id)
                .map(|e| e.key().clone())
                .collect();
            for call_id in to_close {
                if let Some((_, ctx)) = self.active.remove(&call_id) {
                    self.rtp.close(ctx.media_port);
                }
            }
            warn!(device = device_id, reason, "gb28181 device offline");
        }
    }

    /// Expire devices whose registration / keep-alive has lapsed.
    fn expire_devices(&self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .devices
            .iter()
            .filter(|e| now.duration_since(e.value().last_seen) > e.value().expires)
            .map(|e| e.key().clone())
            .collect();
        for id in expired {
            self.unregister_device(&id, "expired");
        }
    }

    fn spawn_maintainer(self: &Arc<Self>) {
        let s = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                s.expire_devices();
            }
        });
    }

    /// Run the SIP receive loop.
    pub async fn run(self: Arc<Self>) {
        self.spawn_maintainer();
        let mut buf = vec![0u8; 4096];
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                info!("gb28181 sip receive loop exiting");
                break;
            }
            match tokio::time::timeout(Duration::from_millis(500), self.socket.recv_from(&mut buf)).await
            {
                Ok(Ok((n, addr))) => {
                    if let Some(msg) = parse_sip(&buf[..n]) {
                        if let Some(method) = &msg.method {
                            match method.as_str() {
                                "REGISTER" => self.handle_register(addr, &msg),
                                "MESSAGE" => self.handle_message(addr, &msg),
                                "INVITE" => self.handle_invite_received(addr, &msg).await,
                                "BYE" => self.handle_bye(addr, &msg),
                                "ACK" | "CANCEL" => {}
                                _ => debug!(method, "unhandled sip method"),
                            }
                        } else {
                            self.handle_response(addr, &msg);
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("sip recv error: {e}");
                    break;
                }
                Err(_) => continue,
            }
        }
    }

    /// Snapshot of all registered devices with their channels.
    pub fn list_devices(&self) -> Vec<DeviceListItem> {
        let now = Instant::now();
        self.devices
            .iter()
            .map(|e| {
                let (id, d) = (e.key().clone(), e.value());
                let since = now.duration_since(d.last_seen);
                DeviceListItem {
                    online: since < d.expires,
                    device_id: id,
                    ip: d.addr.ip().to_string(),
                    port: d.addr.port(),
                    last_seen_ms: since.as_millis() as u64,
                    expires_ms: d.expires.as_millis() as u64,
                    name: d.info.as_ref().and_then(|i| i.name.clone()),
                    manufacturer: d.info.as_ref().and_then(|i| i.manufacturer.clone()),
                    model: d.info.as_ref().and_then(|i| i.model.clone()),
                    firmware: d.info.as_ref().and_then(|i| i.firmware.clone()),
                    channels: d.channels.clone(),
                }
            })
            .collect()
    }

    /// Channels currently cached for a device (from the last `Catalog`).
    pub fn list_channels(&self, device_id: &str) -> Vec<ChannelInfo> {
        self.devices
            .get(device_id)
            .map(|d| d.channels.clone())
            .unwrap_or_default()
    }

    /// Snapshot of the SIP server itself.
    pub fn info(&self) -> SipInfo {
        SipInfo {
            sip_port: self.local_port(),
            realm: self.config.realm.clone(),
            host: self.host(),
            device_count: self.devices.len(),
            active_streams: self.active.len(),
            stopped: self.is_stopped(),
        }
    }

    /// Send a `BYE` to the device for the active or pending stream matching the
    /// given channel id, and close the local RTP port. Best-effort: returns
    /// `false` when no such stream exists.
    pub fn bye_stream(&self, channel_id: &str) -> bool {
        let entry = self
            .active
            .iter()
            .find(|e| e.value().channel_id == channel_id)
            .map(|e| (e.key().clone(), e.value().clone()));
        if let Some((call_id, ctx)) = entry {
            if let Some(device) = self.devices.get(&ctx.device_id) {
                let msg = format!(
                    "BYE sip:{}@{}:{} SIP/2.0\r\n\
                     Via: SIP/2.0/UDP {};branch=z9hG4bK{}\r\n\
                     From: <sip:{}@{}>;tag={}\r\n\
                     To: <sip:{}@{}>\r\n\
                     Call-ID: {}\r\n\
                     CSeq: 2 BYE\r\n\
                     Max-Forwards: 70\r\n\
                     Content-Length: 0\r\n\r\n",
                    ctx.channel_id,
                    device.addr.ip(),
                    device.addr.port(),
                    self.host(),
                    gen_tag(),
                    self.config.realm,
                    self.host(),
                    gen_tag(),
                    ctx.device_id,
                    device.addr.ip(),
                    call_id
                );
                let _ = self.socket.try_send_to(msg.as_bytes(), device.addr);
            }
            self.active.remove(&call_id);
            self.rtp.close(ctx.media_port);
            info!(channel = channel_id, "gb28181 bye sent, stream stopped");
            return true;
        }
        false
    }

    /// A single device snapshot, if registered.
    pub fn get_device(&self, device_id: &str) -> Option<DeviceListItem> {
        let now = Instant::now();
        let (id, d) = self
            .devices
            .get(device_id)
            .map(|e| (e.key().clone(), e.value().clone()))?;
        let since = now.duration_since(d.last_seen);
        Some(DeviceListItem {
            online: since < d.expires,
            device_id: id,
            ip: d.addr.ip().to_string(),
            port: d.addr.port(),
            last_seen_ms: since.as_millis() as u64,
            expires_ms: d.expires.as_millis() as u64,
            name: d.info.as_ref().and_then(|i| i.name.clone()),
            manufacturer: d.info.as_ref().and_then(|i| i.manufacturer.clone()),
            model: d.info.as_ref().and_then(|i| i.model.clone()),
            firmware: d.info.as_ref().and_then(|i| i.firmware.clone()),
            channels: d.channels.clone(),
        })
    }
}

/// Verify a SIP Digest (`MD5`) authorization header.
///
/// `auth` looks like: `Digest username="...", realm="...", nonce="...",
/// uri="...", response="..."`.
fn check_digest(auth: &str, user: &str, password: &str, uri: &str, method: &str, realm: &str) -> bool {
    let mut map: HashMap<String, String> = HashMap::new();
    let inner = auth.strip_prefix("Digest").unwrap_or(auth);
    for part in inner.split(',') {
        if let Some(pos) = part.find('=') {
            let k = part[..pos].trim().to_ascii_lowercase();
            let v = part[pos + 1..].trim().trim_matches('"').to_string();
            map.insert(k, v);
        }
    }
    let response = match map.get("response") {
        Some(r) => r.to_ascii_lowercase(),
        None => return false,
    };
    let realm = map.get("realm").cloned().unwrap_or_else(|| realm.to_string());
    let nonce = match map.get("nonce") {
        Some(n) => n.clone(),
        None => return false,
    };
    let uri = map.get("uri").cloned().unwrap_or_else(|| uri.to_string());
    let a1 = format!("{}:{}:{}", user, realm, password);
    let a2 = format!("{}:{}", method, uri);
    let expected = md5_hex(&format!(
        "{}:{}:{}",
        md5_hex(&a1),
        nonce,
        md5_hex(&a2)
    ));
    expected == response
}

fn gen_ssrc(channel_id: &str) -> u32 {
    let mut h: u32 = 0x1234_5678;
    for b in channel_id.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    h % 1_000_000_000
}

/// Best-effort detection of the primary non-loopback IPv4 address.
fn local_ip() -> String {
    match std::net::UdpSocket::bind("8.8.8.8:80") {
        Ok(s) => match s.local_addr() {
            Ok(addr) => addr.ip().to_string(),
            Err(_) => "127.0.0.1".to_string(),
        },
        Err(_) => "127.0.0.1".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_register() {
        let msg = b"REGISTER sip:34020000002000000001@192.168.1.10:5060 SIP/2.0\r\n\
                     Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bKabc\r\n\
                     From: <sip:34020000001320000001@192.168.1.10:5060>;tag=1\r\n\
                     To: <sip:34020000001320000001@192.168.1.10:5060>\r\n\
                     Call-ID: 1@192.168.1.50\r\n\
                     CSeq: 1 REGISTER\r\n\
                     Content-Length: 0\r\n\r\n";
        let parsed = parse_sip(msg).unwrap();
        assert_eq!(parsed.method.as_deref(), Some("REGISTER"));
        assert_eq!(
            uri_user(parsed.get("from").unwrap()),
            Some("34020000001320000001".to_string())
        );
    }

    #[test]
    fn test_parse_invite_sdp() {
        let msg = b"INVITE sip:34020000001320000001@192.168.1.10 SIP/2.0\r\n\
                     Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9\r\n\
                     From: <sip:34020000002000000001@192.168.1.10>;tag=1\r\n\
                     To: <sip:34020000001320000001@192.168.1.10>\r\n\
                     Call-ID: x@1\r\n\
                     CSeq: 1 INVITE\r\n\
                     Content-Type: application/sdp\r\n\
                     Content-Length: 50\r\n\r\n\
                     v=0\r\n\
                     y=123456789\r\n\
                     m=video 9000 RTP/AVP 96\r\n";
        let parsed = parse_sip(msg).unwrap();
        assert_eq!(parsed.method.as_deref(), Some("INVITE"));
        assert_eq!(sdp_value(&parsed.body, "y="), Some("123456789".to_string()));
    }

    #[test]
    fn test_uri_user() {
        assert_eq!(
            uri_user("<sip:34020000001320000001@1.2.3.4:5060>"),
            Some("34020000001320000001".to_string())
        );
        assert_eq!(
            uri_user("sip:dev@host"),
            Some("dev".to_string())
        );
    }

    #[test]
    fn test_channel_from_xml() {
        let xml = r#"<Response>
<CmdType>Catalog</CmdType>
<SN>7</SN>
<DeviceID>34020000001320000001</DeviceID>
<DeviceList Num="1">
<Item>
<DeviceID>34020000001320000002</DeviceID>
<Name>Front Gate</Name>
<Manufacturer>Hikvision</Manufacturer>
<Model>DS-2CD</Model>
<Status>ON</Status>
<PTZType>1</PTZType>
<Longitude>120.5</Longitude>
<Latitude>30.25</Latitude>
</Item>
</DeviceList>
</Response>"#;
        let root = parse_xml(xml).unwrap();
        let channels = parse_catalog(&root);
        assert_eq!(channels.len(), 1);
        let ch = &channels[0];
        assert_eq!(ch.channel_id, "34020000001320000002");
        assert_eq!(ch.name, "Front Gate");
        assert_eq!(ch.status, "ON");
        assert_eq!(ch.ptz_type, Some(1));
        assert_eq!(ch.longitude, Some(120.5));
        assert_eq!(ch.latitude, Some(30.25));
    }

    #[test]
    fn test_device_info_from_xml() {
        let xml = r#"<Response>
<CmdType>DeviceInfo</CmdType>
<SN>9</SN>
<DeviceID>34020000001320000001</DeviceID>
<DeviceName>NVR-1</DeviceName>
<Manufacturer>Hikvision</Manufacturer>
<Model>iDS-9600</Model>
<Firmware>V5.3.5</Firmware>
<Channel>4</Channel>
<Result>OK</Result>
</Response>"#;
        let root = parse_xml(xml).unwrap();
        let info = DeviceInfo::from_xml(&root);
        assert_eq!(info.device_id, "34020000001320000001");
        assert_eq!(info.name.as_deref(), Some("NVR-1"));
        assert_eq!(info.manufacturer.as_deref(), Some("Hikvision"));
        assert_eq!(info.channel_num, Some(4));
        assert_eq!(info.result.as_deref(), Some("OK"));
    }

    #[test]
    fn test_digest() {
        let realm = "3402000000".to_string();
        // Build the auth header manually so the server can verify it.
        let uri = "sip:34020000002000000001@1.2.3.4:5060";
        let user = "34020000001320000001";
        let password = "12345";
        let nonce = "abc123";
        let a1 = format!("{user}:{realm}:{password}");
        let a2 = format!("REGISTER:{uri}");
        let response = md5_hex(&format!(
            "{}:{}:{}",
            md5_hex(&a1),
            nonce,
            md5_hex(&a2)
        ));
        let auth = format!(
            "Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\""
        );
        assert!(check_digest(&auth, user, password, uri, "REGISTER", &realm));
        assert!(!check_digest(&auth, user, "wrong", uri, "REGISTER", &realm));
    }

    fn sip_msg(
        method: &str,
        dev_addr: SocketAddr,
        sip_port: u16,
        call_id: &str,
        cseq: u32,
        expires: &str,
        body: &str,
    ) -> String {
        format!(
            "{method} sip:34020000002000000001@127.0.0.1:{sip_port} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {dev_addr};branch=z9hG4bKtest\r\n\
             From: <sip:34020000001320000001@127.0.0.1:{sip_port}>;tag=1\r\n\
             To: <sip:34020000001320000001@127.0.0.1:{sip_port}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Contact: <sip:34020000001320000001@{dev_addr}>\r\n\
             Expires: {expires}\r\n\
             Content-Type: Application/MANSCDP+xml\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn test_register_keepalive_unregister_roundtrip() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let cfg = SipServerConfig {
            sip_port: 0,
            realm: "3402000000".to_string(),
            password: None,
            host: Some("127.0.0.1".to_string()),
            media_port_base: 20000,
            vhost: "__defaultVhost__".to_string(),
        };
        let server = SipServer::new(cfg, mgr, 20000).await.unwrap();
        let sip_addr = server.socket.local_addr().unwrap();
        let run = server.clone();
        let handle = tokio::spawn(async move {
            run.run().await;
        });

        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dev_addr = dev.local_addr().unwrap();
        let mut buf = [0u8; 4096];
        async fn recv_resp(dev: &UdpSocket, buf: &mut [u8; 4096]) -> String {
            let (n, _) = tokio::time::timeout(Duration::from_secs(2), dev.recv_from(buf))
                .await
                .expect("timed out waiting for sip response")
                .expect("sip recv error");
            String::from_utf8_lossy(&buf[..n]).to_string()
        }

        // REGISTER.
        dev.send_to(
            sip_msg("REGISTER", dev_addr, sip_addr.port(), "dev1@host", 1, "3600", "").as_bytes(),
            sip_addr,
        )
        .await
        .unwrap();
        assert!(recv_resp(&dev, &mut buf).await.starts_with("SIP/2.0 200"));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let devices = server.list_devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "34020000001320000001");
        assert!(devices[0].online);
        assert_eq!(devices[0].ip, dev_addr.ip().to_string());

        // MESSAGE Keepalive with Interval=30 -> expires becomes 90s.
        let keepalive = "<?xml version=\"1.0\"?><Notify><CmdType>Keepalive</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID><Status>OK</Status><Interval>30</Interval></Notify>";
        dev.send_to(
            sip_msg("MESSAGE", dev_addr, sip_addr.port(), "dev1@host", 2, "3600", keepalive)
                .as_bytes(),
            sip_addr,
        )
        .await
        .unwrap();
        assert!(recv_resp(&dev, &mut buf).await.starts_with("SIP/2.0 200"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.list_devices()[0].expires_ms, 90_000);

        // MESSAGE Catalog with a channel, then verify it is cached.
        let catalog = r#"<?xml version="1.0"?><Response><CmdType>Catalog</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID><DeviceList Num="1"><Item><DeviceID>34020000001320000002</DeviceID><Name>Cam</Name><Status>ON</Status></Item></DeviceList></Response>"#;
        dev.send_to(
            sip_msg("MESSAGE", dev_addr, sip_addr.port(), "dev1@host", 3, "3600", catalog)
                .as_bytes(),
            sip_addr,
        )
        .await
        .unwrap();
        assert!(recv_resp(&dev, &mut buf).await.starts_with("SIP/2.0 200"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let channels = server.list_channels("34020000001320000001");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].channel_id, "34020000001320000002");

        // Un-register (Expires: 0).
        dev.send_to(
            sip_msg("REGISTER", dev_addr, sip_addr.port(), "dev1@host", 4, "0", "").as_bytes(),
            sip_addr,
        )
        .await
        .unwrap();
        assert!(recv_resp(&dev, &mut buf).await.starts_with("SIP/2.0 200"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(server.list_devices().is_empty());

        handle.abort();
    }
}
