//! RTP server manager for receiving raw RTP streams (GB28181 PS, MPEG-TS,
//! raw H.264/H.265, etc.) and publishing them as `MediaSource`s.
//!
//! This backs the `openRtpServer` HTTP API: the caller opens a UDP port and
//! a device sends RTP packets to it; the receiver demuxes the payload and
//! republishes the frames so they can be consumed over any protocol.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{error, info};
use zlmediakit_codec::ps::{extract_nalus_from_pes, parse_one_pes};
use zlmediakit_core::{media_frame::CodecId, MediaFrame, MediaSourceManager};

/// Payload type carried inside the RTP packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtpPayloadType {
    /// MPEG-PS (GB28181 default, payload type 96).
    Ps,
    /// MPEG-TS.
    Ts,
    /// Raw H.264 over RTP (RFC 6184).
    H264,
    /// Raw H.265 over RTP.
    H265,
    /// AAC over RTP (RFC 3640).
    Aac,
    /// G.711 A-law over RTP.
    G711A,
    /// G.711 µ-law over RTP.
    G711U,
    /// Opaque payload, published as H.264 (fallback).
    Raw,
}

impl RtpPayloadType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RtpPayloadType::Ps => "ps",
            RtpPayloadType::Ts => "ts",
            RtpPayloadType::H264 => "h264",
            RtpPayloadType::H265 => "h265",
            RtpPayloadType::Aac => "aac",
            RtpPayloadType::G711A => "g711a",
            RtpPayloadType::G711U => "g711u",
            RtpPayloadType::Raw => "raw",
        }
    }

    pub fn from_str(s: &str) -> RtpPayloadType {
        match s.to_ascii_lowercase().as_str() {
            "ps" => RtpPayloadType::Ps,
            "ts" => RtpPayloadType::Ts,
            "h264" | "h26x" => RtpPayloadType::H264,
            "h265" | "hevc" => RtpPayloadType::H265,
            "aac" => RtpPayloadType::Aac,
            "g711a" | "pcma" => RtpPayloadType::G711A,
            "g711u" | "pcmu" => RtpPayloadType::G711U,
            _ => RtpPayloadType::Raw,
        }
    }

    fn clock_rate(&self) -> u32 {
        match self {
            RtpPayloadType::Ps | RtpPayloadType::Ts | RtpPayloadType::H264 | RtpPayloadType::H265 => {
                90_000
            }
            RtpPayloadType::Aac => 48_000,
            RtpPayloadType::G711A | RtpPayloadType::G711U => 8_000,
            RtpPayloadType::Raw => 90_000,
        }
    }
}

/// Snapshot of a running RTP server, returned by list/get API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtpServerInfo {
    pub port: u16,
    pub ssrc: Option<u32>,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub payload_type: String,
    pub bytes: u64,
    pub packets: u64,
    pub last_recv_ms: u64,
    pub alive: bool,
    pub stream_exists: bool,
}

#[derive(Default)]
struct RtpStats {
    bytes: AtomicU64,
    packets: AtomicU64,
    ssrc: AtomicU32,
    last_recv: Mutex<Option<Instant>>,
}

/// A single open RTP reception port.
pub struct RtpStreamReceiver {
    socket: Arc<UdpSocket>,
    vhost: String,
    app: String,
    stream: String,
    payload: RtpPayloadType,
    audio_codec: CodecId,
    expected_ssrc: Option<u32>,
    manager: Arc<MediaSourceManager>,
    stats: Arc<RtpStats>,
    shutdown: Arc<AtomicBool>,
}

impl RtpStreamReceiver {
    fn new(
        socket: Arc<UdpSocket>,
        vhost: String,
        app: String,
        stream: String,
        payload: RtpPayloadType,
        expected_ssrc: Option<u32>,
        manager: Arc<MediaSourceManager>,
    ) -> Self {
        let audio_codec = match payload {
            RtpPayloadType::G711U => CodecId::G711U,
            _ => CodecId::G711A,
        };
        Self {
            socket,
            vhost,
            app,
            stream,
            payload,
            audio_codec,
            expected_ssrc,
            manager,
            stats: Arc::new(RtpStats::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn mark_alive(&self) {
        let stats = self.stats.clone();
        tokio::spawn(async move {
            *stats.last_recv.lock().await = Some(Instant::now());
        });
    }

    async fn publish_video(&self, nalu: &[u8], rtp_ts: u32, video_codec: &mut CodecId) {
        if nalu.len() < 5 {
            return;
        }
        let h264_type = nalu[4] & 0x1F;
        let h265_type = (nalu[4] >> 1) & 0x3F;
        // Disambiguate H.264 vs H.265 from SPS/PPS / slice types.
        if h264_type == 7 || h264_type == 8 || (h264_type >= 1 && h264_type <= 5) {
            *video_codec = CodecId::H264;
        } else if h265_type == 32 || h265_type == 33 || h265_type == 34 {
            *video_codec = CodecId::H265;
        }
        let key = h264_type == 5 || (h265_type >= 19 && h265_type <= 21);
        let clock = self.payload.clock_rate();
        let pts_ms = (rtp_ts as u64) * 1000 / clock as u64;
        let frame = MediaFrame::new_video(
            0,
            *video_codec,
            pts_ms as u32,
            pts_ms,
            pts_ms,
            Bytes::copy_from_slice(nalu),
            key,
        );
        self.publish(frame).await;
    }

    async fn publish_audio(&self, data: Bytes, rtp_ts: u32) {
        let clock = self.payload.clock_rate();
        let pts_ms = (rtp_ts as u64) * 1000 / clock as u64;
        let frame = MediaFrame::new_audio(
            1,
            self.audio_codec,
            pts_ms as u32,
            pts_ms,
            pts_ms,
            data,
        );
        self.publish(frame).await;
    }

    async fn publish(&self, frame: MediaFrame) {
        let src = self
            .manager
            .get_or_create(&self.vhost, &self.app, &self.stream);
        src.publish_and_cache(frame).await;
    }

    async fn handle_rtp(
        &self,
        pkt: &[u8],
        ps_buf: &mut BytesMut,
        fu_buf: &mut BytesMut,
        video_codec: &mut CodecId,
    ) {
        if pkt.len() < 12 {
            return;
        }
        let b0 = pkt[0];
        let version = b0 >> 6;
        if version != 2 {
            return;
        }
        let padding = (b0 >> 5) & 0x1 == 1;
        let extension = (b0 >> 4) & 0x1 == 1;
        let csrc_count = (b0 & 0x0F) as usize;
        let marker = (pkt[1] >> 7) & 0x1 == 1;
        let _pt = pkt[1] & 0x7F;
        let _seq = u16::from_be_bytes([pkt[2], pkt[3]]);
        let rtp_ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
        let ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
        self.stats.ssrc.store(ssrc, Ordering::SeqCst);

        if let Some(exp) = self.expected_ssrc {
            if exp != ssrc {
                return;
            }
        }

        // Skip CSRC and optional extension header.
        let mut off = 12 + csrc_count * 4;
        if extension && off + 4 <= pkt.len() {
            let ext_len = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize * 4;
            off += 4 + ext_len;
        }
        if off >= pkt.len() {
            return;
        }
        let mut payload = &pkt[off..];
        if padding && !payload.is_empty() {
            let pad_len = *payload.last().unwrap() as usize;
            if pad_len <= payload.len() {
                payload = &payload[..payload.len() - pad_len];
            }
        }
        if payload.is_empty() {
            return;
        }

        match self.payload {
            RtpPayloadType::Ps => {
                ps_buf.extend_from_slice(payload);
                if marker {
                    self.process_ps(ps_buf, rtp_ts, video_codec).await;
                    ps_buf.clear();
                }
            }
            RtpPayloadType::Ts => {
                // MPEG-TS is handled at the srt crate level; forward as raw H.264
                // fallback is not appropriate, so ignore here.
                let _ = marker;
            }
            RtpPayloadType::H264 | RtpPayloadType::H265 | RtpPayloadType::Raw => {
                self.handle_h26x(payload, rtp_ts, fu_buf, video_codec).await;
            }
            RtpPayloadType::Aac => {
                self.publish_audio(Bytes::copy_from_slice(payload), rtp_ts).await;
            }
            RtpPayloadType::G711A | RtpPayloadType::G711U => {
                self.publish_audio(Bytes::copy_from_slice(payload), rtp_ts).await;
            }
        }
    }

    async fn handle_h26x(
        &self,
        payload: &[u8],
        rtp_ts: u32,
        fu_buf: &mut BytesMut,
        video_codec: &mut CodecId,
    ) {
        if payload.is_empty() {
            return;
        }
        let nal_header = payload[0];
        let nal_type = nal_header & 0x1F;
        if nal_type == 28 {
            // FU-A fragmentation unit.
            if payload.len() < 2 {
                return;
            }
            let fu_hdr = payload[1];
            let start = (fu_hdr & 0x80) != 0;
            let end = (fu_hdr & 0x40) != 0;
            let real_type = fu_hdr & 0x1F;
            if start {
                fu_buf.clear();
                fu_buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                fu_buf.extend_from_slice(&[(nal_header & 0xE0) | real_type]);
            }
            fu_buf.extend_from_slice(&payload[2..]);
            if end {
                let data = fu_buf.split().freeze();
                self.publish_video(&data, rtp_ts, video_codec).await;
            }
        } else if nal_type == 24 {
            // STAP-A aggregation.
            let mut p = 1;
            while p + 2 <= payload.len() {
                let len = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
                p += 2;
                if p + len > payload.len() {
                    break;
                }
                let mut nalu = Vec::with_capacity(len + 4);
                nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                nalu.extend_from_slice(&payload[p..p + len]);
                self.publish_video(&nalu, rtp_ts, video_codec).await;
                p += len;
            }
        } else {
            let mut nalu = Vec::with_capacity(payload.len() + 4);
            nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            nalu.extend_from_slice(payload);
            self.publish_video(&nalu, rtp_ts, video_codec).await;
        }
    }

    async fn process_ps(&self, ps: &[u8], rtp_ts: u32, video_codec: &mut CodecId) {
        let mut data = ps;
        while !data.is_empty() {
            match parse_one_pes(data) {
                Ok(Some((pes, consumed))) => {
                    if pes.stream_id == 0xE0 || (pes.stream_id & 0xF0) == 0xE0 {
                        // Video ES.
                        let pts = pes.pts.unwrap_or((rtp_ts as u64) * 1000 / 90);
                        let _ = pts;
                        for nalu in extract_nalus_from_pes(&pes.data) {
                            self.publish_video(&nalu, rtp_ts, video_codec).await;
                        }
                    } else if pes.stream_id == 0xC0 || (pes.stream_id & 0xF0) == 0xC0 {
                        // Audio ES.
                        self.publish_audio(pes.data.clone(), rtp_ts).await;
                    } else if pes.stream_id == 0xBD {
                        // Private stream (often audio in PS).
                        self.publish_audio(pes.data.clone(), rtp_ts).await;
                    }
                    if consumed == 0 {
                        break;
                    }
                    data = &data[consumed..];
                }
                Ok(None) | Err(_) => break,
            }
        }
    }

    async fn run(self: Arc<Self>) {
        let mut buf = vec![0u8; 2048];
        let mut ps_buf = BytesMut::new();
        let mut fu_buf = BytesMut::new();
        let mut video_codec = CodecId::H264;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(500), self.socket.recv_from(&mut buf)).await
            {
                Ok(Ok((n, _addr))) => {
                    self.stats.bytes.fetch_add(n as u64, Ordering::SeqCst);
                    self.stats.packets.fetch_add(1, Ordering::SeqCst);
                    self.mark_alive();
                    self.handle_rtp(&buf[..n], &mut ps_buf, &mut fu_buf, &mut video_codec)
                        .await;
                }
                Ok(Err(e)) => {
                    error!(port = self.socket.local_addr().map(|a| a.port()).unwrap_or(0), "rtp recv error: {e}");
                    break;
                }
                Err(_) => continue,
            }
        }
        info!(stream = %self.stream, "rtp server stopped");
    }

    fn info(&self, port: u16) -> RtpServerInfo {
        let ssrc = if self.expected_ssrc.is_some() {
            self.expected_ssrc
        } else {
            let v = self.stats.ssrc.load(Ordering::SeqCst);
            if v == 0 {
                None
            } else {
                Some(v)
            }
        };
        let last_recv_ms = {
            let guard = self.stats.last_recv.blocking_lock();
            guard
                .map(|i| i.elapsed().as_millis() as u64)
                .unwrap_or(u64::MAX)
        };
        let stream_exists = self
            .manager
            .get(&self.vhost, &self.app, &self.stream)
            .is_some();
        RtpServerInfo {
            port,
            ssrc,
            vhost: self.vhost.clone(),
            app: self.app.clone(),
            stream: self.stream.clone(),
            payload_type: self.payload.as_str().to_string(),
            bytes: self.stats.bytes.load(Ordering::SeqCst),
            packets: self.stats.packets.load(Ordering::SeqCst),
            last_recv_ms,
            alive: last_recv_ms < 5000,
            stream_exists,
        }
    }
}

/// Manages all open RTP reception ports.
#[derive(Clone)]
pub struct RtpServerManager {
    inner: Arc<DashMap<u16, Arc<RtpStreamReceiver>>>,
    next_port: Arc<AtomicU16>,
    manager: Arc<MediaSourceManager>,
}

impl RtpServerManager {
    pub fn new(manager: Arc<MediaSourceManager>, media_port_base: u16) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(DashMap::new()),
            next_port: Arc::new(AtomicU16::new(media_port_base.max(1))),
            manager,
        })
    }

    fn alloc_port(&self) -> u16 {
        loop {
            let p = self.next_port.fetch_add(1, Ordering::SeqCst);
            if p == 0 {
                continue;
            }
            if !self.inner.contains_key(&p) {
                return p;
            }
        }
    }

    /// Open a new RTP reception port. `port == 0` auto-allocates one.
    /// Returns the actual bound port.
    pub async fn open(
        &self,
        port: u16,
        vhost: &str,
        app: &str,
        stream: &str,
        payload: RtpPayloadType,
        ssrc: Option<u32>,
    ) -> anyhow::Result<u16> {
        let port = if port == 0 { self.alloc_port() } else { port };
        if self.inner.contains_key(&port) {
            anyhow::bail!("rtp server on port {port} already exists");
        }
        let socket = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await?);
        let recv = RtpStreamReceiver::new(
            socket,
            vhost.to_string(),
            app.to_string(),
            stream.to_string(),
            payload,
            ssrc,
            self.manager.clone(),
        );
        let recv = Arc::new(recv);
        let task = recv.clone();
        tokio::spawn(async move {
            task.run().await;
        });
        self.inner.insert(port, recv);
        info!(port, stream, "rtp server opened");
        Ok(port)
    }

    /// Close an open RTP reception port and stop publishing.
    pub fn close(&self, port: u16) {
        if let Some((_, recv)) = self.inner.remove(&port) {
            recv.shutdown.store(true, Ordering::SeqCst);
            info!(port, "rtp server closed");
        }
    }

    pub fn list(&self) -> Vec<RtpServerInfo> {
        self.inner
            .iter()
            .map(|e| e.value().info(*e.key()))
            .collect()
    }

    pub fn get_info(&self, port: u16) -> Option<RtpServerInfo> {
        self.inner.get(&port).map(|r| r.info(port))
    }

    /// Find the port (and info) opened for a given app/stream, if any.
    pub fn find_by_stream(&self, app: &str, stream: &str) -> Option<RtpServerInfo> {
        self.inner.iter().find_map(|e| {
            let info = e.value().info(*e.key());
            if info.app == app && info.stream == stream {
                Some(info)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zlmediakit_core::MediaSourceManager;

    #[test]
    fn test_payload_type_roundtrip() {
        assert_eq!(RtpPayloadType::from_str("ps"), RtpPayloadType::Ps);
        assert_eq!(RtpPayloadType::from_str("h265"), RtpPayloadType::H265);
        assert_eq!(RtpPayloadType::from_str("g711a"), RtpPayloadType::G711A);
        assert_eq!(RtpPayloadType::from_str("raw").as_str(), "raw");
    }

    #[tokio::test]
    async fn test_rtp_h264_single_nalu_publishes_frame() {
        let mgr = std::sync::Arc::new(MediaSourceManager::new());
        let rtp = RtpServerManager::new(mgr.clone(), 30000);
        let port = rtp
            .open(0, "__defaultVhost__", "rtp", "h264u", RtpPayloadType::H264, None)
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "h264u");
        let mut rx = src.subscribe();

        // RTP header: v2, marker=1, pt=96, seq=1, ts=0, ssrc
        let mut pkt = vec![0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78];
        // H.264 IDR single NALU: nal_header type 5 (0x65) + payload
        pkt.extend_from_slice(&[0x65, 0x01, 0x02, 0x03, 0x04]);

        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        sock.send_to(&pkt, ("127.0.0.1", port)).unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for frame")
            .expect("no frame received");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_open_close_list() {
        let mgr = std::sync::Arc::new(MediaSourceManager::new());
        let rtp = RtpServerManager::new(mgr.clone(), 31000);
        let p = rtp
            .open(31010, "__defaultVhost__", "rtp", "c1", RtpPayloadType::Ps, None)
            .await
            .unwrap();
        assert_eq!(p, 31010);
        assert!(rtp.get_info(31010).is_some());
        rtp.close(31010);
        assert!(rtp.get_info(31010).is_none());
    }
}
