//! RTP server manager for receiving raw RTP streams (GB28181 PS, MPEG-TS,
//! raw H.264/H.265, etc.) and publishing them as `MediaSource`s.
//!
//! This backs the `openRtpServer` HTTP API: the caller opens a UDP port and
//! a device sends RTP packets to it; the receiver demuxes the payload and
//! republishes the frames so they can be consumed over any protocol.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::{lookup_host, TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};
use zlmediakit_codec::ps::{extract_nalus_from_pes, PesPacket, PsDemuxer};
use zlmediakit_core::{
    media_frame::{CodecId, PayloadFormat},
    MediaFrame, MediaSourceManager,
};

use crate::server::{demux_ts_packets, TsContext};

/// Payload type carried inside the RTP packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtpPayloadType {
    /// Detect PS, TS, H.26x, or static audio payloads from the first RTP packet.
    Auto,
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
            RtpPayloadType::Auto => "auto",
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

    pub fn parse(s: &str) -> RtpPayloadType {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => RtpPayloadType::Auto,
            "ps" => RtpPayloadType::Ps,
            "ts" => RtpPayloadType::Ts,
            "h264" | "h26x" => RtpPayloadType::H264,
            "h265" | "hevc" => RtpPayloadType::H265,
            "aac" => RtpPayloadType::Aac,
            "g711a" | "pcma" => RtpPayloadType::G711A,
            "g711u" | "pcmu" => RtpPayloadType::G711U,
            "raw" => RtpPayloadType::Raw,
            _ => RtpPayloadType::Auto,
        }
    }

    fn clock_rate(&self) -> u32 {
        match self {
            RtpPayloadType::Auto
            | RtpPayloadType::Ps
            | RtpPayloadType::Ts
            | RtpPayloadType::H264
            | RtpPayloadType::H265 => 90_000,
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
    pub lost_packets: u64,
    pub duplicate_packets: u64,
    pub reordered_packets: u64,
    pub last_recv_ms: u64,
    pub alive: bool,
    pub stream_exists: bool,
    pub tcp_mode: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpTcpMode {
    Udp = 0,
    TcpPassive = 1,
    TcpActive = 2,
}

impl TryFrom<u8> for RtpTcpMode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> anyhow::Result<Self> {
        match value {
            0 => Ok(Self::Udp),
            1 => Ok(Self::TcpPassive),
            2 => Ok(Self::TcpActive),
            _ => anyhow::bail!("tcp_mode must be 0 (UDP), 1 (TCP passive), or 2 (TCP active)"),
        }
    }
}

#[derive(Default)]
struct RtpStats {
    bytes: AtomicU64,
    packets: AtomicU64,
    lost_packets: AtomicU64,
    duplicate_packets: AtomicU64,
    reordered_packets: AtomicU64,
    ssrc: AtomicU32,
    last_recv: Mutex<Option<Instant>>,
}

struct PendingRtpPacket {
    packet: Bytes,
    arrived_at: Instant,
}

/// Small sequence-number jitter buffer shared by UDP and TCP RTP ingest.
/// Packets are released in order, duplicates/late packets are discarded, and
/// a bounded window prevents a missing packet from stalling the stream.
struct RtpReorderBuffer {
    expected: Option<u16>,
    pending: HashMap<u16, PendingRtpPacket>,
    window: u16,
    max_delay: Duration,
}

impl Default for RtpReorderBuffer {
    fn default() -> Self {
        Self {
            expected: None,
            pending: HashMap::new(),
            window: 64,
            max_delay: Duration::from_millis(50),
        }
    }
}

impl RtpReorderBuffer {
    fn push(&mut self, packet: Bytes, stats: &RtpStats) -> Vec<Bytes> {
        if packet.len() < 4 {
            return Vec::new();
        }
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let Some(expected) = self.expected else {
            self.expected = Some(sequence.wrapping_add(1));
            return vec![packet];
        };
        let distance = sequence.wrapping_sub(expected);
        if distance == 0 {
            let mut ready = vec![packet];
            self.expected = Some(expected.wrapping_add(1));
            self.drain_ready(&mut ready);
            return ready;
        }
        if distance >= 0x8000 || self.pending.contains_key(&sequence) {
            stats.duplicate_packets.fetch_add(1, Ordering::Relaxed);
            return Vec::new();
        }

        stats.reordered_packets.fetch_add(1, Ordering::Relaxed);
        self.pending.insert(
            sequence,
            PendingRtpPacket {
                packet,
                arrived_at: Instant::now(),
            },
        );
        let timed_out = self
            .pending
            .values()
            .any(|pending| pending.arrived_at.elapsed() >= self.max_delay);
        if distance >= self.window || timed_out || self.pending.len() >= self.window as usize {
            self.skip_gap(stats)
        } else {
            Vec::new()
        }
    }

    fn skip_gap(&mut self, stats: &RtpStats) -> Vec<Bytes> {
        let expected = self.expected.expect("initialized before buffering");
        let Some((&nearest, _)) = self
            .pending
            .iter()
            .filter(|(sequence, _)| sequence.wrapping_sub(expected) < 0x8000)
            .min_by_key(|(sequence, _)| sequence.wrapping_sub(expected))
        else {
            return Vec::new();
        };
        let lost = nearest.wrapping_sub(expected) as u64;
        stats.lost_packets.fetch_add(lost, Ordering::Relaxed);
        self.expected = Some(nearest);
        let mut ready = Vec::new();
        self.drain_ready(&mut ready);
        ready
    }

    fn drain_ready(&mut self, ready: &mut Vec<Bytes>) {
        while let Some(expected) = self.expected {
            let Some(pending) = self.pending.remove(&expected) else {
                break;
            };
            ready.push(pending.packet);
            self.expected = Some(expected.wrapping_add(1));
        }
    }
}

/// A single open RTP reception port.
enum RtpInput {
    Udp(Arc<UdpSocket>),
    TcpPassive(Arc<TcpListener>),
    TcpActive(Arc<TcpActiveInput>),
}

struct TcpActiveInput {
    local_addr: SocketAddr,
    initial_socket: Mutex<Option<TcpSocket>>,
    target: Mutex<Option<SocketAddr>>,
    target_changed: Notify,
}

impl RtpInput {
    fn tcp_mode(&self) -> RtpTcpMode {
        match self {
            Self::Udp(_) => RtpTcpMode::Udp,
            Self::TcpPassive(_) => RtpTcpMode::TcpPassive,
            Self::TcpActive(_) => RtpTcpMode::TcpActive,
        }
    }
}

pub struct RtpStreamReceiver {
    input: RtpInput,
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

struct RtpIngestState {
    ps_demuxer: PsDemuxer,
    ts_context: TsContext,
    ts_timestamp: u32,
    detected_payload: Option<RtpPayloadType>,
    fu_buf: BytesMut,
    video_codec: CodecId,
    reorder: RtpReorderBuffer,
}

impl Default for RtpIngestState {
    fn default() -> Self {
        Self {
            ps_demuxer: PsDemuxer::new(),
            ts_context: TsContext::new(),
            ts_timestamp: 0,
            detected_payload: None,
            fu_buf: BytesMut::new(),
            video_codec: CodecId::H264,
            reorder: RtpReorderBuffer::default(),
        }
    }
}

impl RtpStreamReceiver {
    fn new(
        input: RtpInput,
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
            input,
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

    async fn publish_video(&self, nalu: &[u8], pts_ms: u64, video_codec: &mut CodecId) {
        if nalu.len() < 5 {
            return;
        }
        let h264_type = nalu[4] & 0x1F;
        let h265_type = (nalu[4] >> 1) & 0x3F;
        // Disambiguate H.264 vs H.265 from SPS/PPS / slice types.
        if h264_type == 7 || h264_type == 8 || (1..=5).contains(&h264_type) {
            *video_codec = CodecId::H264;
        } else if h265_type == 32 || h265_type == 33 || h265_type == 34 {
            *video_codec = CodecId::H265;
        }
        let key = h264_type == 5 || (19..=21).contains(&h265_type);
        let frame = MediaFrame::new_video(
            0,
            *video_codec,
            pts_ms as u32,
            pts_ms,
            pts_ms,
            Bytes::copy_from_slice(nalu),
            key,
        )
        .with_payload_format(PayloadFormat::AnnexB);
        self.publish(frame).await;
    }

    async fn publish_audio(&self, data: Bytes, pts_ms: u64, codec: CodecId) {
        let format = if codec == CodecId::AAC {
            if data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0 {
                PayloadFormat::Adts
            } else {
                PayloadFormat::AacRaw
            }
        } else {
            PayloadFormat::Raw
        };
        let frame = MediaFrame::new_audio(1, codec, pts_ms as u32, pts_ms, pts_ms, data)
            .with_payload_format(format);
        self.publish(frame).await;
    }

    fn rtp_ts_to_ms(&self, rtp_ts: u32) -> u64 {
        let clock = self.payload.clock_rate();
        (rtp_ts as u64) * 1000 / clock as u64
    }

    fn detect_payload_type(payload: &[u8], payload_type: u8) -> RtpPayloadType {
        if payload_type == 0 {
            return RtpPayloadType::G711U;
        }
        if payload_type == 8 {
            return RtpPayloadType::G711A;
        }
        if payload.starts_with(&[0x00, 0x00, 0x01])
            && payload.get(3).is_some_and(|id| matches!(id, 0xBA..=0xBF))
        {
            return RtpPayloadType::Ps;
        }
        if payload.len() >= 188
            && payload[0] == 0x47
            && (payload.len() < 376 || payload[188] == 0x47)
        {
            return RtpPayloadType::Ts;
        }
        if payload.len() >= 2 {
            let h265_type = (payload[0] >> 1) & 0x3F;
            if matches!(h265_type, 32..=34 | 48 | 49) {
                return RtpPayloadType::H265;
            }
        }
        RtpPayloadType::H264
    }

    async fn publish(&self, frame: MediaFrame) {
        let src = self
            .manager
            .get_or_create(&self.vhost, &self.app, &self.stream);
        src.publish_and_cache(frame).await;
    }

    async fn handle_rtp(&self, pkt: &[u8], state: &mut RtpIngestState) {
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
        let payload_type = pkt[1] & 0x7F;
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

        let effective_payload = if self.payload == RtpPayloadType::Auto {
            *state
                .detected_payload
                .get_or_insert_with(|| Self::detect_payload_type(payload, payload_type))
        } else {
            self.payload
        };

        match effective_payload {
            RtpPayloadType::Auto => unreachable!("auto payload is resolved above"),
            RtpPayloadType::Ps => {
                // GB28181 cameras frequently omit the RTP marker bit, so the
                // PES payload is accumulated and parsed incrementally. Any
                // PES packets that are split across RTP packets (length == 0
                // video PES) are completed as soon as the next entity arrives.
                state.ps_demuxer.push(payload);
                while let Some(pes) = state.ps_demuxer.next_pes() {
                    let stream_type = state.ps_demuxer.stream_type(pes.stream_id);
                    self.process_pes(&pes, stream_type, rtp_ts, &mut state.video_codec)
                        .await;
                }
            }
            RtpPayloadType::Ts => {
                let source = self
                    .manager
                    .get_or_create(&self.vhost, &self.app, &self.stream);
                demux_ts_packets(
                    payload,
                    &mut state.ts_context,
                    &source,
                    &mut state.ts_timestamp,
                )
                .await;
            }
            RtpPayloadType::H264 | RtpPayloadType::H265 | RtpPayloadType::Raw => {
                self.handle_h26x(
                    payload,
                    rtp_ts,
                    effective_payload,
                    &mut state.fu_buf,
                    &mut state.video_codec,
                )
                .await;
            }
            RtpPayloadType::Aac => {
                let pts_ms = (rtp_ts as u64) * 1000 / effective_payload.clock_rate() as u64;
                self.publish_aac_rtp(payload, pts_ms).await;
            }
            RtpPayloadType::G711A | RtpPayloadType::G711U => {
                let pts_ms = (rtp_ts as u64) * 1000 / effective_payload.clock_rate() as u64;
                let codec = if effective_payload == RtpPayloadType::G711U {
                    CodecId::G711U
                } else {
                    CodecId::G711A
                };
                self.publish_audio(Bytes::copy_from_slice(payload), pts_ms, codec)
                    .await;
            }
        }
    }

    async fn publish_aac_rtp(&self, payload: &[u8], pts_ms: u64) {
        if payload.len() >= 4 {
            let header_bits = u16::from_be_bytes([payload[0], payload[1]]) as usize;
            let header_bytes = header_bits.div_ceil(8);
            if header_bits >= 16
                && header_bits.is_multiple_of(16)
                && 2 + header_bytes <= payload.len()
            {
                let mut data_offset = 2 + header_bytes;
                for header in payload[2..2 + header_bytes].chunks_exact(2) {
                    let au_size = (u16::from_be_bytes([header[0], header[1]]) >> 3) as usize;
                    if au_size == 0 || data_offset + au_size > payload.len() {
                        return;
                    }
                    self.publish_audio(
                        Bytes::copy_from_slice(&payload[data_offset..data_offset + au_size]),
                        pts_ms,
                        CodecId::AAC,
                    )
                    .await;
                    data_offset += au_size;
                }
                return;
            }
        }
        self.publish_audio(Bytes::copy_from_slice(payload), pts_ms, CodecId::AAC)
            .await;
    }

    async fn handle_h26x(
        &self,
        payload: &[u8],
        rtp_ts: u32,
        payload_type: RtpPayloadType,
        fu_buf: &mut BytesMut,
        video_codec: &mut CodecId,
    ) {
        if payload.is_empty() {
            return;
        }
        if payload_type == RtpPayloadType::H265 {
            *video_codec = CodecId::H265;
            self.handle_h265(payload, rtp_ts, fu_buf, video_codec).await;
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
                let pts_ms = self.rtp_ts_to_ms(rtp_ts);
                self.publish_video(&data, pts_ms, video_codec).await;
            }
        } else if nal_type == 24 {
            // STAP-A aggregation.
            let mut p = 1;
            let pts_ms = self.rtp_ts_to_ms(rtp_ts);
            while p + 2 <= payload.len() {
                let len = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
                p += 2;
                if p + len > payload.len() {
                    break;
                }
                let mut nalu = Vec::with_capacity(len + 4);
                nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                nalu.extend_from_slice(&payload[p..p + len]);
                self.publish_video(&nalu, pts_ms, video_codec).await;
                p += len;
            }
        } else {
            let mut nalu = Vec::with_capacity(payload.len() + 4);
            nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            nalu.extend_from_slice(payload);
            let pts_ms = self.rtp_ts_to_ms(rtp_ts);
            self.publish_video(&nalu, pts_ms, video_codec).await;
        }
    }

    async fn handle_h265(
        &self,
        payload: &[u8],
        rtp_ts: u32,
        fu_buf: &mut BytesMut,
        video_codec: &mut CodecId,
    ) {
        if payload.len() < 2 {
            return;
        }
        let nal_type = (payload[0] >> 1) & 0x3F;
        if nal_type == 49 {
            if payload.len() < 3 {
                return;
            }
            let fu_header = payload[2];
            let start = fu_header & 0x80 != 0;
            let end = fu_header & 0x40 != 0;
            let original_type = fu_header & 0x3F;
            if start {
                fu_buf.clear();
                fu_buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                fu_buf.extend_from_slice(&[(payload[0] & 0x81) | (original_type << 1), payload[1]]);
            }
            if !fu_buf.is_empty() {
                fu_buf.extend_from_slice(&payload[3..]);
            }
            if end && !fu_buf.is_empty() {
                let data = fu_buf.split().freeze();
                self.publish_video(&data, self.rtp_ts_to_ms(rtp_ts), video_codec)
                    .await;
            }
        } else if nal_type == 48 {
            let mut offset = 2;
            let pts_ms = self.rtp_ts_to_ms(rtp_ts);
            while offset + 2 <= payload.len() {
                let length = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if offset + length > payload.len() {
                    break;
                }
                let mut nalu = Vec::with_capacity(length + 4);
                nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                nalu.extend_from_slice(&payload[offset..offset + length]);
                self.publish_video(&nalu, pts_ms, video_codec).await;
                offset += length;
            }
        } else {
            let mut nalu = Vec::with_capacity(payload.len() + 4);
            nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            nalu.extend_from_slice(payload);
            self.publish_video(&nalu, self.rtp_ts_to_ms(rtp_ts), video_codec)
                .await;
        }
    }

    async fn process_pes(
        &self,
        pes: &PesPacket,
        stream_type: Option<u8>,
        rtp_ts: u32,
        video_codec: &mut CodecId,
    ) {
        // Prefer the PES PTS (90kHz); fall back to the RTP timestamp.
        let pts_ms = match pes.pts {
            Some(pts) => pts * 1000 / 90_000,
            None => self.rtp_ts_to_ms(rtp_ts),
        };
        if pes.stream_id == 0xE0 || (pes.stream_id & 0xF0) == 0xE0 {
            // Video ES.
            if stream_type == Some(0x24) {
                *video_codec = CodecId::H265;
            } else if stream_type == Some(0x1B) {
                *video_codec = CodecId::H264;
            }
            for nalu in extract_nalus_from_pes(&pes.data) {
                self.publish_video(&nalu, pts_ms, video_codec).await;
            }
        } else if pes.stream_id == 0xC0 || (pes.stream_id & 0xF0) == 0xC0 {
            // Audio ES.
            let codec = Self::ps_audio_codec(stream_type).unwrap_or(self.audio_codec);
            self.publish_audio(pes.data.clone(), pts_ms, codec).await;
        } else if pes.stream_id == 0xBD {
            // Private stream (often audio in PS).
            let codec = Self::ps_audio_codec(stream_type).unwrap_or(self.audio_codec);
            self.publish_audio(pes.data.clone(), pts_ms, codec).await;
        }
    }

    fn ps_audio_codec(stream_type: Option<u8>) -> Option<CodecId> {
        match stream_type? {
            0x0F | 0x11 => Some(CodecId::AAC),
            0x90 => Some(CodecId::G711A),
            0x91 => Some(CodecId::G711U),
            0x04 => Some(CodecId::MP2A),
            0x03 => Some(CodecId::Mp3),
            _ => None,
        }
    }

    async fn run(self: Arc<Self>) {
        match &self.input {
            RtpInput::Udp(socket) => self.run_udp(socket.clone()).await,
            RtpInput::TcpPassive(listener) => self.run_tcp_passive(listener.clone()).await,
            RtpInput::TcpActive(input) => self.run_tcp_active(input.clone()).await,
        }
        info!(stream = %self.stream, "rtp server stopped");
    }

    async fn ingest_packet(&self, packet: &[u8], state: &mut RtpIngestState) {
        self.stats
            .bytes
            .fetch_add(packet.len() as u64, Ordering::SeqCst);
        self.stats.packets.fetch_add(1, Ordering::SeqCst);
        self.mark_alive();
        for packet in state
            .reorder
            .push(Bytes::copy_from_slice(packet), &self.stats)
        {
            self.handle_rtp(&packet, state).await;
        }
    }

    async fn run_udp(&self, socket: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 2048];
        let mut state = RtpIngestState::default();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await
            {
                Ok(Ok((n, _addr))) => {
                    self.ingest_packet(&buf[..n], &mut state).await;
                }
                Ok(Err(e)) => {
                    error!(
                        port = socket.local_addr().map(|a| a.port()).unwrap_or(0),
                        "rtp recv error: {e}"
                    );
                    break;
                }
                Err(_) => continue,
            }
        }
    }

    async fn run_tcp_passive(&self, listener: Arc<TcpListener>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let accepted =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
            let mut stream = match accepted {
                Ok(Ok((stream, peer))) => {
                    info!(%peer, stream = %self.stream, "RTP/TCP client connected");
                    stream
                }
                Ok(Err(error)) => {
                    error!("RTP/TCP accept error: {error}");
                    continue;
                }
                Err(_) => continue,
            };
            self.read_tcp_stream(&mut stream).await;
        }
    }

    async fn run_tcp_active(&self, input: Arc<TcpActiveInput>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let target = loop {
                if let Some(target) = *input.target.lock().await {
                    break target;
                }
                if self.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let _ = tokio::time::timeout(
                    Duration::from_millis(500),
                    input.target_changed.notified(),
                )
                .await;
            };

            let socket = match input.initial_socket.lock().await.take() {
                Some(socket) => socket,
                None => match Self::create_bound_tcp_socket(input.local_addr) {
                    Ok(socket) => socket,
                    Err(error) => {
                        error!(%error, local = %input.local_addr, "failed to recreate RTP/TCP socket");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
            };
            match socket.connect(target).await {
                Ok(mut stream) => {
                    info!(%target, stream = %self.stream, "RTP/TCP active connection established");
                    self.read_tcp_stream(&mut stream).await;
                }
                Err(error) => {
                    warn!(%error, %target, stream = %self.stream, "RTP/TCP active connect failed");
                }
            }
            if !self.shutdown.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    fn create_bound_tcp_socket(local_addr: SocketAddr) -> anyhow::Result<TcpSocket> {
        let socket = if local_addr.is_ipv6() {
            TcpSocket::new_v6()?
        } else {
            TcpSocket::new_v4()?
        };
        socket.set_reuseaddr(true)?;
        socket.bind(local_addr)?;
        Ok(socket)
    }

    async fn set_tcp_active_target(&self, target: SocketAddr) -> anyhow::Result<()> {
        let RtpInput::TcpActive(input) = &self.input else {
            anyhow::bail!("RTP server is not in TCP active mode");
        };
        *input.target.lock().await = Some(target);
        input.target_changed.notify_one();
        Ok(())
    }

    async fn read_tcp_stream(&self, stream: &mut TcpStream) {
        let mut state = RtpIngestState::default();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let mut length = [0u8; 2];
            match tokio::time::timeout(Duration::from_millis(500), stream.read_exact(&mut length))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => return,
                Err(_) => continue,
            }
            let length = u16::from_be_bytes(length) as usize;
            if length < 12 {
                return;
            }
            let mut packet = vec![0u8; length];
            if stream.read_exact(&mut packet).await.is_err() {
                return;
            }
            self.ingest_packet(&packet, &mut state).await;
        }
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
        let last_recv_ms = match self.stats.last_recv.try_lock() {
            Ok(guard) => guard
                .map(|i| i.elapsed().as_millis() as u64)
                .unwrap_or(u64::MAX),
            Err(_) => u64::MAX,
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
            lost_packets: self.stats.lost_packets.load(Ordering::SeqCst),
            duplicate_packets: self.stats.duplicate_packets.load(Ordering::SeqCst),
            reordered_packets: self.stats.reordered_packets.load(Ordering::SeqCst),
            last_recv_ms,
            alive: last_recv_ms < 5000,
            stream_exists,
            tcp_mode: self.input.tcp_mode() as u8,
        }
    }
}

/// Manages all open RTP reception ports.
#[derive(Clone)]
pub struct RtpServerManager {
    inner: Arc<DashMap<u16, Arc<RtpStreamReceiver>>>,
    next_port: Arc<AtomicU16>,
    manager: Arc<MediaSourceManager>,
    bind_ip: std::net::IpAddr,
}

impl RtpServerManager {
    pub fn new(manager: Arc<MediaSourceManager>, media_port_base: u16) -> Arc<Self> {
        Self::new_with_bind_ip(
            manager,
            media_port_base,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        )
    }

    pub fn new_with_bind_ip(
        manager: Arc<MediaSourceManager>,
        media_port_base: u16,
        bind_ip: std::net::IpAddr,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(DashMap::new()),
            next_port: Arc::new(AtomicU16::new(media_port_base.max(1))),
            manager,
            bind_ip,
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
        self.open_with_mode(port, vhost, app, stream, payload, ssrc, RtpTcpMode::Udp)
            .await
    }

    /// Open an RTP reception port using the requested ZLMediaKit-compatible
    /// transport mode (`0` UDP, `1` TCP passive, `2` TCP active). Active mode
    /// reserves the local port and waits for [`Self::connect_stream`] to supply
    /// the destination.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_with_mode(
        &self,
        port: u16,
        vhost: &str,
        app: &str,
        stream: &str,
        payload: RtpPayloadType,
        ssrc: Option<u32>,
        tcp_mode: RtpTcpMode,
    ) -> anyhow::Result<u16> {
        let port = if port == 0 { self.alloc_port() } else { port };
        if self.inner.contains_key(&port) {
            anyhow::bail!("rtp server on port {port} already exists");
        }
        let local_addr = SocketAddr::new(self.bind_ip, port);
        let input = match tcp_mode {
            RtpTcpMode::Udp => RtpInput::Udp(Arc::new(UdpSocket::bind(local_addr).await?)),
            RtpTcpMode::TcpPassive => {
                RtpInput::TcpPassive(Arc::new(TcpListener::bind(local_addr).await?))
            }
            RtpTcpMode::TcpActive => {
                let socket = RtpStreamReceiver::create_bound_tcp_socket(local_addr)?;
                RtpInput::TcpActive(Arc::new(TcpActiveInput {
                    local_addr,
                    initial_socket: Mutex::new(Some(socket)),
                    target: Mutex::new(None),
                    target_changed: Notify::new(),
                }))
            }
        };
        let recv = RtpStreamReceiver::new(
            input,
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
        info!(port, stream, tcp_mode = tcp_mode as u8, "rtp server opened");
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

    /// Supply the remote endpoint for an RTP server opened in TCP active mode.
    /// The receiver reconnects automatically after a connection failure.
    pub async fn connect_stream(
        &self,
        app: Option<&str>,
        stream: &str,
        dst_host: &str,
        dst_port: u16,
    ) -> anyhow::Result<u16> {
        let selected = self.inner.iter().find_map(|entry| {
            let receiver = entry.value();
            if receiver.stream == stream && app.is_none_or(|app| receiver.app == app) {
                Some((*entry.key(), receiver.clone()))
            } else {
                None
            }
        });
        let Some((local_port, receiver)) = selected else {
            anyhow::bail!("RTP server for stream '{stream}' was not found");
        };
        let target = lookup_host((dst_host, dst_port))
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("unable to resolve RTP destination {dst_host}"))?;
        receiver.set_tcp_active_target(target).await?;
        Ok(local_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use zlmediakit_core::rtp::{RtpDataType, RtpSendRequest, RtpSenderManager};
    use zlmediakit_core::MediaSourceManager;

    #[test]
    fn test_payload_type_roundtrip() {
        assert_eq!(RtpPayloadType::parse("auto"), RtpPayloadType::Auto);
        assert_eq!(RtpPayloadType::parse("ps"), RtpPayloadType::Ps);
        assert_eq!(RtpPayloadType::parse("h265"), RtpPayloadType::H265);
        assert_eq!(RtpPayloadType::parse("g711a"), RtpPayloadType::G711A);
        assert_eq!(RtpPayloadType::parse("raw").as_str(), "raw");
        assert_eq!(
            RtpStreamReceiver::detect_payload_type(&[0x47; 188], 96),
            RtpPayloadType::Ts
        );
        assert_eq!(
            RtpStreamReceiver::detect_payload_type(&[0, 0, 1, 0xBA], 96),
            RtpPayloadType::Ps
        );
        assert_eq!(
            RtpStreamReceiver::detect_payload_type(&[0x62, 0x01, 0x93], 96),
            RtpPayloadType::H265
        );
    }

    fn sequence_packet(sequence: u16) -> Bytes {
        Bytes::from(vec![0x80, 96, (sequence >> 8) as u8, sequence as u8])
    }

    fn packet_sequence(packet: &Bytes) -> u16 {
        u16::from_be_bytes([packet[2], packet[3]])
    }

    #[test]
    fn reorder_buffer_orders_packets_and_drops_duplicates() {
        let stats = RtpStats::default();
        let mut reorder = RtpReorderBuffer::default();
        assert_eq!(
            packet_sequence(&reorder.push(sequence_packet(10), &stats)[0]),
            10
        );
        assert!(reorder.push(sequence_packet(12), &stats).is_empty());
        assert!(reorder.push(sequence_packet(12), &stats).is_empty());
        let ready = reorder.push(sequence_packet(11), &stats);
        assert_eq!(
            ready.iter().map(packet_sequence).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert_eq!(stats.reordered_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.duplicate_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.lost_packets.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reorder_buffer_handles_wraparound_and_bounded_loss() {
        let stats = RtpStats::default();
        let mut wrap = RtpReorderBuffer::default();
        assert_eq!(
            packet_sequence(&wrap.push(sequence_packet(u16::MAX), &stats)[0]),
            u16::MAX
        );
        assert_eq!(
            packet_sequence(&wrap.push(sequence_packet(0), &stats)[0]),
            0
        );

        let mut loss = RtpReorderBuffer {
            window: 2,
            ..Default::default()
        };
        assert_eq!(
            packet_sequence(&loss.push(sequence_packet(1), &stats)[0]),
            1
        );
        let ready = loss.push(sequence_packet(4), &stats);
        assert_eq!(packet_sequence(&ready[0]), 4);
        assert_eq!(stats.lost_packets.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_rtp_h264_single_nalu_publishes_frame() {
        let mgr = std::sync::Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30000);
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "h264u",
                RtpPayloadType::H264,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "h264u");
        let mut rx = src.subscribe();

        // RTP header: v2, marker=1, pt=96, seq=1, ts=0, ssrc
        let mut pkt = vec![
            0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78,
        ];
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
    async fn test_rtp_ipv6_udp_listener_publishes_frame() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new_with_bind_ip(mgr.clone(), 30050, "::1".parse().unwrap());
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "h264_ipv6",
                RtpPayloadType::H264,
                None,
            )
            .await
            .unwrap();
        let source = mgr.get_or_create("__defaultVhost__", "rtp", "h264_ipv6");
        let mut frames = source.subscribe();
        let mut packet = vec![
            0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78,
        ];
        packet.extend_from_slice(&[0x65, 0x01, 0x02, 0x03, 0x04]);
        let socket = UdpSocket::bind("[::1]:0").await.unwrap();
        socket.send_to(&packet, ("::1", port)).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("timed out waiting for IPv6 RTP frame")
            .expect("IPv6 RTP listener closed");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_received_rtp_can_be_relayed_by_common_sender() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let receiver_manager = RtpServerManager::new(mgr.clone(), 30100);
        let inbound_port = receiver_manager
            .open(
                0,
                "__defaultVhost__",
                "gb28181",
                "relay_source",
                RtpPayloadType::H264,
                None,
            )
            .await
            .unwrap();

        let mut inbound_packet = vec![
            0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78,
        ];
        inbound_packet.extend_from_slice(&[0x65, 0x11, 0x22, 0x33, 0x44]);
        let input = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        input
            .send_to(&inbound_packet, ("127.0.0.1", inbound_port))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while mgr
                .get("__defaultVhost__", "gb28181", "relay_source")
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("GB/RTP receiver did not publish its media source");

        // The generic RTP sender subscribes to the media source created by the
        // GB/RTP receiver. This is the controlled protocol-relay path exposed by
        // startSendRtp: received RTP -> MediaSource -> a new RTP destination.
        let destination = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination_port = destination.local_addr().unwrap().port();
        let sender_manager = RtpSenderManager::new(mgr.clone());
        sender_manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".to_string(),
                app: "gb28181".to_string(),
                stream: "relay_source".to_string(),
                ssrc: 0x8765_4321,
                dst_host: "127.0.0.1".to_string(),
                dst_port: destination_port,
                src_port: 0,
                payload_type: 98,
                is_udp: true,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: false,
            })
            .await
            .unwrap();

        let mut outbound_packet = [0u8; 1500];
        let size = tokio::time::timeout(
            Duration::from_secs(2),
            destination.recv(&mut outbound_packet),
        )
        .await
        .expect("timed out waiting for relayed RTP")
        .unwrap();
        assert!(size >= 17);
        assert_eq!(outbound_packet[1] & 0x7f, 98);
        assert_eq!(
            u32::from_be_bytes(outbound_packet[8..12].try_into().unwrap()),
            0x8765_4321
        );
        assert_eq!(&outbound_packet[12..size], &[0x65, 0x11, 0x22, 0x33, 0x44]);

        assert_eq!(
            sender_manager.stop(
                "__defaultVhost__",
                "gb28181",
                "relay_source",
                Some(0x8765_4321),
            ),
            1
        );
        receiver_manager.close(inbound_port);
    }

    #[tokio::test]
    async fn test_auto_detects_h265_fu_and_reassembles_frame() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30200);
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "h265_auto",
                RtpPayloadType::Auto,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "h265_auto");
        let mut rx = src.subscribe();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let mut start = vec![0x80, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78];
        start.extend_from_slice(&[0x62, 0x01, 0x93, 0xAA, 0xBB]);
        let mut end = vec![0x80, 0xE0, 0x00, 0x02, 0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78];
        end.extend_from_slice(&[0x62, 0x01, 0x53, 0xCC, 0xDD]);
        socket.send_to(&start, ("127.0.0.1", port)).await.unwrap();
        socket.send_to(&end, ("127.0.0.1", port)).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for H.265 FU frame")
            .expect("no H.265 FU frame received");
        assert_eq!(frame.codec, CodecId::H265);
        assert!(frame.key_frame);
        assert_eq!(&frame.data[..6], &[0, 0, 0, 1, 0x26, 0x01]);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_auto_detects_ts_and_demuxes_video_pes() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30300);
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "ts_auto",
                RtpPayloadType::Auto,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "ts_auto");
        let mut rx = src.subscribe();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        fn ts_video_packet(continuity: u8, nalu_type: u8) -> Vec<u8> {
            let mut packet = vec![0xFF; 188];
            packet[..4].copy_from_slice(&[0x47, 0x41, 0x00, 0x10 | continuity]);
            let payload = [
                0, 0, 1, 0xE0, 0, 0, 0x80, 0, 0, 0, 0, 0, 1, nalu_type, 1, 2, 3, 4,
            ];
            packet[4..4 + payload.len()].copy_from_slice(&payload);
            packet
        }

        for (sequence, ts_packet) in [ts_video_packet(0, 0x65), ts_video_packet(1, 0x41)]
            .into_iter()
            .enumerate()
        {
            let mut rtp_packet = vec![
                0x80,
                0x60,
                0,
                sequence as u8 + 1,
                0,
                0,
                0,
                0,
                0xCA,
                0xFE,
                0xBA,
                0xBE,
            ];
            rtp_packet.extend_from_slice(&ts_packet);
            socket
                .send_to(&rtp_packet, ("127.0.0.1", port))
                .await
                .unwrap();
        }

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for TS video frame")
            .expect("no TS video frame received");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_psm_maps_aac_audio_codec() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30400);
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "ps_aac",
                RtpPayloadType::Auto,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "ps_aac");
        let mut rx = src.subscribe();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let mut ps = vec![
            0, 0, 1, 0xBC, 0, 14, 0xE0, 0xFF, 0, 0, 0, 4, 0x0F, 0xC0, 0, 0, 0, 0, 0, 0,
        ];
        let adts = [0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x7F, 0xFC];
        ps.extend_from_slice(&[0, 0, 1, 0xC0, 0, 10, 0x80, 0, 0]);
        ps.extend_from_slice(&adts);
        let mut packet = vec![0x80, 0xE0, 0, 1, 0, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF];
        packet.extend_from_slice(&ps);
        socket.send_to(&packet, ("127.0.0.1", port)).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for PS AAC frame")
            .expect("no PS AAC frame received");
        assert_eq!(frame.codec, CodecId::AAC);
        assert_eq!(frame.payload_format, PayloadFormat::Adts);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_aac_rfc3640_au_header_is_removed() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30450);
        let port = rtp
            .open(
                0,
                "__defaultVhost__",
                "rtp",
                "aac_au",
                RtpPayloadType::Aac,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "aac_au");
        let mut rx = src.subscribe();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut packet = vec![0x80, 0xE0, 0, 1, 0, 0, 0, 0, 0xAA, 0xBB, 0xCC, 0xDD];
        // AU-headers-length=16, one AU header with a 4-byte AAC access unit.
        packet.extend_from_slice(&[0, 16, 0, 32, 1, 2, 3, 4]);
        socket.send_to(&packet, ("127.0.0.1", port)).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for AAC AU")
            .expect("no AAC AU received");
        assert_eq!(frame.codec, CodecId::AAC);
        assert_eq!(frame.payload_format, PayloadFormat::AacRaw);
        assert_eq!(&frame.data[..], &[1, 2, 3, 4]);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_rtp_tcp_passive_rfc4571_publishes_frame() {
        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30500);
        let port = rtp
            .open_with_mode(
                0,
                "__defaultVhost__",
                "rtp",
                "h264_tcp",
                RtpPayloadType::H264,
                None,
                RtpTcpMode::TcpPassive,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "h264_tcp");
        let mut rx = src.subscribe();

        let mut packet = vec![
            0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x34, 0x56, 0x78,
        ];
        packet.extend_from_slice(&[0x65, 0x01, 0x02, 0x03, 0x04]);
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(&(packet.len() as u16).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&packet).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for TCP RTP frame")
            .expect("no TCP RTP frame received");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        let info = rtp.get_info(port).unwrap();
        assert_eq!(info.tcp_mode, RtpTcpMode::TcpPassive as u8);
        assert_eq!(info.packets, 1);
        rtp.close(port);
    }

    #[tokio::test]
    async fn test_rtp_tcp_active_connects_and_publishes_frame() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let sender = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut packet = vec![
                0x80, 0xE0, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x87, 0x65, 0x43, 0x21,
            ];
            packet.extend_from_slice(&[0x65, 0x11, 0x22, 0x33, 0x44]);
            stream
                .write_all(&(packet.len() as u16).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&packet).await.unwrap();
        });

        let mgr = Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 30600);
        let local_port = rtp
            .open_with_mode(
                0,
                "__defaultVhost__",
                "rtp",
                "h264_active",
                RtpPayloadType::H264,
                None,
                RtpTcpMode::TcpActive,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "h264_active");
        let mut rx = src.subscribe();
        assert_eq!(
            rtp.connect_stream(Some("rtp"), "h264_active", "127.0.0.1", upstream_port)
                .await
                .unwrap(),
            local_port
        );

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for active TCP RTP frame")
            .expect("no active TCP RTP frame received");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        assert_eq!(rtp.get_info(local_port).unwrap().tcp_mode, 2);
        sender.await.unwrap();
        rtp.close(local_port);
    }

    #[tokio::test]
    async fn test_open_close_list() {
        let mgr = std::sync::Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 31000);
        let p = rtp
            .open(
                31010,
                "__defaultVhost__",
                "rtp",
                "c1",
                RtpPayloadType::Ps,
                None,
            )
            .await
            .unwrap();
        assert_eq!(p, 31010);
        assert!(rtp.get_info(31010).is_some());
        rtp.close(31010);
        assert!(rtp.get_info(31010).is_none());
    }

    #[tokio::test]
    async fn test_rtp_ps_split_across_packets_no_marker() {
        // GB28181 cameras may spread one PES across several RTP packets and
        // omit the marker bit entirely. The incremental PsDemuxer must still
        // recover the video frames.
        let mgr = std::sync::Arc::new(MediaSourceManager::new(None));
        let rtp = RtpServerManager::new(mgr.clone(), 32000);
        let port = rtp
            .open(
                32020,
                "__defaultVhost__",
                "rtp",
                "psu",
                RtpPayloadType::Ps,
                None,
            )
            .await
            .unwrap();
        let src = mgr.get_or_create("__defaultVhost__", "rtp", "psu");
        let mut rx = src.subscribe();

        // Build a PS stream: pack header + video PES (length == 0) + pack header
        // terminator. The payload is an H.264 IDR NAL (Annex-B).
        let mut ps = Vec::new();
        ps.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x02, 0x03, 0xF8,
        ]);
        let nalu = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, 0x03, 0x04];
        // PES header: 00 00 01 E0, length 0, PTS-only flags, 5-byte PTS (9000
        // ticks @90kHz = 100ms: 0x21 0x00 0x01 0x46 0x51).
        ps.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x46, 0x51,
        ]);
        ps.extend_from_slice(&nalu);
        ps.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x02, 0x03, 0xF8,
        ]);

        // Split into three RTP packets. Marker bit is ALWAYS 0.
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        for (i, chunk) in ps.chunks(ps.len() / 3 + 1).enumerate() {
            let mut pkt = vec![
                0x80,
                0x60,
                0x00,
                i as u8 + 1,
                0x00,
                0x00,
                0x00,
                0x00,
                0x12,
                0x34,
                0x56,
                0x78,
            ];
            pkt.extend_from_slice(chunk);
            sock.send_to(&pkt, ("127.0.0.1", port)).unwrap();
        }

        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for frame")
            .expect("no frame received");
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
        assert_eq!(frame.pts, 100, "PES PTS should be propagated");
        rtp.close(port);
    }
}
