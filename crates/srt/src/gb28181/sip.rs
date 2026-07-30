//! Minimal GB28181 SIP server (UDP).
//!
//! Implements the subset of SIP needed to act as a GB28181 platform:
//! * `REGISTER` handling with optional Digest authentication.
//! * `MESSAGE` handling (Catalog / DeviceInfo / Keepalive / Notify) — acknowledged.
//! * Platform-initiated `INVITE` to a registered device to request a stream;
//!   the device is told (via the SDP `c=`/`m=` lines) to send RTP/PS to a local
//!   media port which is opened on the fly via [`RtpServerManager`].
//! * Receiving an `INVITE` from a device (push mode) and `BYE` teardown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};
use zlmediakit_core::MediaSourceManager;

use super::rtp_server::{RtpPayloadType, RtpServerManager};

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

#[derive(Debug, Clone)]
struct DeviceContact {
    addr: SocketAddr,
}

#[derive(Debug, Clone)]
struct InviteCtx {
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

/// The SIP server.
pub struct SipServer {
    socket: UdpSocket,
    config: SipServerConfig,
    rtp: Arc<RtpServerManager>,
    devices: DashMap<String, DeviceContact>,
    pending: DashMap<String, InviteCtx>,
    active: DashMap<String, InviteCtx>,
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
        }))
    }

    pub fn rtp_manager(&self) -> Arc<RtpServerManager> {
        self.rtp.clone()
    }

    fn host(&self) -> String {
        if let Some(h) = &self.config.host {
            if !h.is_empty() {
                return h.clone();
            }
        }
        local_ip()
    }

    /// Request a stream from a registered device by channel id.
    /// Opens a local RTP media port and sends an `INVITE`. Returns the media port.
    pub async fn invite(&self, device_id: &str, channel_id: &str) -> anyhow::Result<u16> {
        let device = self
            .devices
            .get(device_id)
            .ok_or_else(|| anyhow::anyhow!("device {device_id} not registered"))?
            .clone();
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
                channel_id: channel_id.to_string(),
                media_port,
            },
        );
        Ok(media_port)
    }

    fn rtp_port_for_invite(&self) -> u16 {
        // Allocate a fresh port via the manager's open(0) path by reserving.
        // We pick an incrementing port above the base to avoid collisions.
        use std::sync::atomic::{AtomicU16, Ordering};
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
                if !self.check_digest(&auth, &device_id, password, &msg.uri, "REGISTER") {
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

        self.devices.insert(
            device_id.clone(),
            DeviceContact {
                addr,
            },
        );
        info!(device = device_id, %addr, "gb28181 device registered");
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
                ("Expires", "3600".to_string()),
            ],
            "",
        );
    }

    fn check_digest(&self, auth: &str, user: &str, password: &str, uri: &str, method: &str) -> bool {
        // auth like: Digest username="...", realm="...", nonce="...", uri="...", response="..."
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
        let realm = map.get("realm").cloned().unwrap_or_else(|| self.config.realm.clone());
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

    fn handle_message(&self, addr: SocketAddr, msg: &SipMessage) {
        let from = msg.get("from").cloned().unwrap_or_default();
        let device_id = uri_user(&from).unwrap_or_default();
        let to = msg.get("to").cloned().unwrap_or_default();
        let call_id = msg.get("call-id").cloned().unwrap_or_default();
        let cseq = msg.get("cseq").cloned().unwrap_or_default();
        let via = msg.get("via").cloned().unwrap_or_default();
        let contact = msg.get("contact").cloned().unwrap_or_default();
        let tag = gen_tag();

        // Learn device contact.
        self.devices.insert(
            device_id.clone(),
            DeviceContact {
                addr,
            },
        );

        // Log the command type (Catalog / Keepalive / Notify / ...).
        let cmd = sdp_value(&msg.body, "CommandType");
        debug!(device = device_id, ?cmd, "gb28181 message");

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

    /// Run the SIP receive loop.
    pub async fn run(self: Arc<Self>) {
        let mut buf = vec![0u8; 4096];
        loop {
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

    pub fn list_devices(&self) -> Vec<String> {
        self.devices.iter().map(|e| e.key().clone()).collect()
    }
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
}
