use crate::media_frame::{CodecId, FrameType, MediaFrame, PayloadFormat, TrackInfo};
use crate::payload::{aac_raw_payload, video_config_annex_b, video_sample_annex_b};
use crate::ps::PsMuxer;
use crate::MediaSourceManager;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use dashmap::DashMap;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::Notify;
use uuid::Uuid;

const DEFAULT_MTU: usize = 1400;

/// Container muxer injected by a higher-level crate when its implementation
/// lives outside `zlmediakit-core` (currently MPEG-TS in `zlmediakit-hls`).
pub trait RtpFrameMuxer: Send {
    fn mux_frame(&mut self, frame: &MediaFrame) -> Result<Vec<u8>>;
}

impl<F> RtpFrameMuxer for F
where
    F: FnMut(&MediaFrame) -> Result<Vec<u8>> + Send,
{
    fn mux_frame(&mut self, frame: &MediaFrame) -> Result<Vec<u8>> {
        self(frame)
    }
}

pub type RtpMuxerFactory = Arc<dyn Fn() -> Box<dyn RtpFrameMuxer> + Send + Sync>;

/// Converts one elementary audio or video track into RTP packets.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    payload_type: u8,
    sequence: u16,
    clock_rate: u32,
    mtu: usize,
}

impl RtpPacketizer {
    pub fn new(ssrc: u32, payload_type: u8, clock_rate: u32) -> Self {
        Self {
            ssrc,
            payload_type: payload_type & 0x7f,
            sequence: 0,
            clock_rate: clock_rate.max(1),
            mtu: DEFAULT_MTU,
        }
    }

    pub fn with_sequence(mut self, sequence: u16) -> Self {
        self.sequence = sequence;
        self
    }

    pub fn with_payload_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu.clamp(1, u16::MAX as usize - 12);
        self
    }

    pub fn packetize(&mut self, frame: &MediaFrame) -> Result<Vec<Vec<u8>>> {
        match frame.frame_type {
            FrameType::Video => self.packetize_video(frame),
            FrameType::Audio => self.packetize_audio(frame),
            FrameType::Metadata => Ok(Vec::new()),
        }
    }

    /// Splits an already muxed payload, such as MPEG-PS, across RTP packets.
    pub fn packetize_payload(&mut self, payload: &[u8], timestamp_ms: u32) -> Vec<Vec<u8>> {
        if payload.is_empty() {
            return Vec::new();
        }
        let timestamp = self.timestamp(timestamp_ms);
        let chunk_count = payload.len().div_ceil(self.mtu);
        payload
            .chunks(self.mtu)
            .enumerate()
            .map(|(index, chunk)| {
                let mut packet = Vec::with_capacity(12 + chunk.len());
                packet.extend_from_slice(&self.header(timestamp, index + 1 == chunk_count));
                packet.extend_from_slice(chunk);
                packet
            })
            .collect()
    }

    fn timestamp(&self, timestamp_ms: u32) -> u32 {
        ((timestamp_ms as u64 * self.clock_rate as u64) / 1000) as u32
    }

    fn header(&mut self, timestamp: u32, marker: bool) -> [u8; 12] {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut header = [0u8; 12];
        header[0] = 0x80;
        header[1] = self.payload_type | if marker { 0x80 } else { 0 };
        header[2..4].copy_from_slice(&sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        header
    }

    fn packetize_video(&mut self, frame: &MediaFrame) -> Result<Vec<Vec<u8>>> {
        if !matches!(frame.codec, CodecId::H264 | CodecId::H265) {
            bail!("RTP ES video codec {:?} is not supported", frame.codec);
        }
        let annex_b = if frame.config_frame {
            video_config_annex_b(frame)?
        } else {
            video_sample_annex_b(frame)?
        };
        let nalus = annex_b_nalus(&annex_b);
        let timestamp = self.timestamp(frame.timestamp);
        let nalu_count = nalus.len();
        let mut packets = Vec::new();
        for (index, nalu) in nalus.into_iter().enumerate() {
            let last_nalu = index + 1 == nalu_count;
            match frame.codec {
                CodecId::H264 => self.packetize_h264_nalu(nalu, timestamp, last_nalu, &mut packets),
                CodecId::H265 => self.packetize_h265_nalu(nalu, timestamp, last_nalu, &mut packets),
                _ => unreachable!(),
            }
        }
        Ok(packets)
    }

    fn packetize_h264_nalu(
        &mut self,
        nalu: &[u8],
        timestamp: u32,
        last_nalu: bool,
        packets: &mut Vec<Vec<u8>>,
    ) {
        if nalu.is_empty() {
            return;
        }
        if nalu.len() <= self.mtu {
            let mut packet = Vec::with_capacity(12 + nalu.len());
            packet.extend_from_slice(&self.header(timestamp, last_nalu));
            packet.extend_from_slice(nalu);
            packets.push(packet);
            return;
        }
        let nal_header = nalu[0];
        let fu_indicator = (nal_header & 0xe0) | 28;
        let nal_type = nal_header & 0x1f;
        let mut offset = 1;
        let mut first = true;
        while offset < nalu.len() {
            let chunk = (self.mtu - 2).min(nalu.len() - offset);
            let end = offset + chunk == nalu.len();
            let fu_header = nal_type | if first { 0x80 } else { 0 } | if end { 0x40 } else { 0 };
            let mut packet = Vec::with_capacity(12 + 2 + chunk);
            packet.extend_from_slice(&self.header(timestamp, last_nalu && end));
            packet.push(fu_indicator);
            packet.push(fu_header);
            packet.extend_from_slice(&nalu[offset..offset + chunk]);
            packets.push(packet);
            offset += chunk;
            first = false;
        }
    }

    fn packetize_h265_nalu(
        &mut self,
        nalu: &[u8],
        timestamp: u32,
        last_nalu: bool,
        packets: &mut Vec<Vec<u8>>,
    ) {
        if nalu.len() < 2 {
            return;
        }
        if nalu.len() <= self.mtu {
            let mut packet = Vec::with_capacity(12 + nalu.len());
            packet.extend_from_slice(&self.header(timestamp, last_nalu));
            packet.extend_from_slice(nalu);
            packets.push(packet);
            return;
        }
        let nal_header0 = nalu[0];
        let nal_header1 = nalu[1];
        let nal_type = (nal_header0 >> 1) & 0x3f;
        let payload_header0 = (nal_header0 & 0x81) | (49 << 1);
        let mut offset = 2;
        let mut first = true;
        while offset < nalu.len() {
            let chunk = (self.mtu - 3).min(nalu.len() - offset);
            let end = offset + chunk == nalu.len();
            let fu_header = nal_type | if first { 0x80 } else { 0 } | if end { 0x40 } else { 0 };
            let mut packet = Vec::with_capacity(12 + 3 + chunk);
            packet.extend_from_slice(&self.header(timestamp, last_nalu && end));
            packet.push(payload_header0);
            packet.push(nal_header1);
            packet.push(fu_header);
            packet.extend_from_slice(&nalu[offset..offset + chunk]);
            packets.push(packet);
            offset += chunk;
            first = false;
        }
    }

    fn packetize_audio(&mut self, frame: &MediaFrame) -> Result<Vec<Vec<u8>>> {
        if frame.config_frame {
            return Ok(Vec::new());
        }
        let (payload, aac, mpeg_audio) = match frame.codec {
            CodecId::AAC => (aac_raw_payload(frame)?, true, false),
            CodecId::Opus | CodecId::G711A | CodecId::G711U | CodecId::L16 => {
                (raw_audio_payload(frame), false, false)
            }
            CodecId::Mp3 | CodecId::MP2A => (raw_audio_payload(frame), false, true),
            _ => bail!("RTP ES audio codec {:?} is not supported", frame.codec),
        };
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let timestamp = self.timestamp(frame.timestamp);
        let extra = if aac || mpeg_audio { 4 } else { 0 };
        let mut packet = Vec::with_capacity(12 + extra + payload.len());
        packet.extend_from_slice(&self.header(timestamp, true));
        if aac {
            packet.extend_from_slice(&16u16.to_be_bytes());
            let au_header = ((payload.len() as u16) << 3) & 0xfff8;
            packet.extend_from_slice(&au_header.to_be_bytes());
        } else if mpeg_audio {
            packet.extend_from_slice(&[0, 0, 0, 0]);
        }
        packet.extend_from_slice(&payload);
        Ok(vec![packet])
    }
}

fn raw_audio_payload(frame: &MediaFrame) -> Bytes {
    let flv = frame.payload_format == PayloadFormat::Flv
        || (frame.payload_format == PayloadFormat::Unknown
            && frame
                .data
                .first()
                .is_some_and(|header| matches!(header >> 4, 2 | 7 | 8)));
    if flv && !frame.data.is_empty() {
        frame.data.slice(1..)
    } else {
        frame.data.clone()
    }
}

fn annex_b_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let start_len = if offset + 4 <= data.len() && data[offset..offset + 4] == [0, 0, 0, 1] {
            4
        } else if data[offset..offset + 3] == [0, 0, 1] {
            3
        } else {
            offset += 1;
            continue;
        };
        starts.push((offset, offset + start_len));
        offset += start_len;
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(index, (_, payload_start))| {
            let end = starts
                .get(index + 1)
                .map(|(start, _)| *start)
                .unwrap_or(data.len());
            (*payload_start < end).then_some(&data[*payload_start..end])
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpDataType {
    Es,
    Ps,
    Ts,
}

impl TryFrom<u8> for RtpDataType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Es),
            1 => Ok(Self::Ps),
            2 => Ok(Self::Ts),
            _ => bail!("RTP data type must be 0 (ES), 1 (PS), or 2 (TS)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpSendRequest {
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub ssrc: u32,
    pub dst_host: String,
    pub dst_port: u16,
    pub src_port: u16,
    pub payload_type: u8,
    pub is_udp: bool,
    pub data_type: RtpDataType,
    pub only_audio: bool,
    pub passive: bool,
    pub ssrc_multi_send: bool,
}

#[derive(Debug, Clone)]
pub struct RtpSenderInfo {
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub ssrc: u32,
    pub destination: Option<SocketAddr>,
    pub local_port: u16,
    pub packets: u64,
    pub bytes: u64,
}

struct RtpSenderEntry {
    token: Uuid,
    request: RtpSendRequest,
    destination: Option<SocketAddr>,
    local_port: u16,
    stop: Arc<Notify>,
    stopped: AtomicBool,
    packets: AtomicU64,
    bytes: AtomicU64,
}

enum RtpTransport {
    Udp(UdpSocket),
    /// RFC 4571 framing: each RTP packet is prefixed by a two-byte length.
    TcpActive {
        stream: Option<TcpStream>,
        destination: SocketAddr,
        src_port: u16,
    },
    TcpPassive {
        listener: TcpListener,
        stream: Option<TcpStream>,
    },
}

impl RtpTransport {
    fn local_port(&self) -> Result<u16> {
        Ok(match self {
            Self::Udp(socket) => socket.local_addr()?.port(),
            Self::TcpActive { stream, .. } => stream
                .as_ref()
                .ok_or_else(|| anyhow!("active RTP/TCP stream is not connected"))?
                .local_addr()?
                .port(),
            Self::TcpPassive { listener, .. } => listener.local_addr()?.port(),
        })
    }

    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        match self {
            Self::Udp(socket) => {
                socket.send(packet).await?;
            }
            Self::TcpActive {
                stream,
                destination,
                src_port,
            } => loop {
                if stream.is_none() {
                    match connect_tcp(*destination, *src_port).await {
                        Ok(connected) => *stream = Some(connected),
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            continue;
                        }
                    }
                }
                if write_tcp_packet(stream.as_mut().expect("checked above"), packet)
                    .await
                    .is_ok()
                {
                    break;
                }
                *stream = None;
            },
            Self::TcpPassive { listener, stream } => loop {
                if stream.is_none() {
                    match listener.accept().await {
                        Ok((accepted, _)) => *stream = Some(accepted),
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                }
                if write_tcp_packet(stream.as_mut().expect("checked above"), packet)
                    .await
                    .is_ok()
                {
                    break;
                }
                *stream = None;
            },
        }
        Ok(())
    }
}

async fn connect_tcp(destination: SocketAddr, src_port: u16) -> Result<TcpStream> {
    let socket = match destination.ip() {
        IpAddr::V4(_) => TcpSocket::new_v4()?,
        IpAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    if src_port != 0 {
        let bind_ip = match destination.ip() {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        };
        socket
            .bind(SocketAddr::new(bind_ip, src_port))
            .context("failed to bind RTP TCP socket")?;
    }
    socket
        .connect(destination)
        .await
        .context("failed to connect RTP TCP destination")
}

fn bind_udp_socket(address: SocketAddr, reuse_port: bool) -> Result<UdpSocket> {
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if reuse_port {
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
    }
    socket.bind(&address.into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into()).context("failed to create RTP UDP socket")
}

async fn write_tcp_packet(stream: &mut TcpStream, packet: &[u8]) -> Result<()> {
    let length = u16::try_from(packet.len())
        .map_err(|_| anyhow!("RTP packet exceeds RFC 4571 frame size"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(packet).await?;
    Ok(())
}

#[derive(Clone)]
pub struct RtpSenderManager {
    source_manager: Arc<MediaSourceManager>,
    active: Arc<DashMap<String, Arc<RtpSenderEntry>>>,
    ts_muxer_factory: Option<RtpMuxerFactory>,
}

impl RtpSenderManager {
    pub fn new(source_manager: Arc<MediaSourceManager>) -> Self {
        Self {
            source_manager,
            active: Arc::new(DashMap::new()),
            ts_muxer_factory: None,
        }
    }

    pub fn with_ts_muxer_factory(mut self, factory: RtpMuxerFactory) -> Self {
        self.ts_muxer_factory = Some(factory);
        self
    }

    fn key(request: &RtpSendRequest) -> String {
        let base = format!(
            "{}/{}/{}/{}",
            request.vhost, request.app, request.stream, request.ssrc
        );
        if request.ssrc_multi_send {
            format!(
                "{base}/{}:{}:{}:{}",
                request.dst_host, request.dst_port, request.src_port, request.passive
            )
        } else {
            base
        }
    }

    pub async fn start(&self, request: RtpSendRequest) -> Result<u16> {
        let ts_muxer_factory = if matches!(request.data_type, RtpDataType::Ts) {
            Some(
                self.ts_muxer_factory
                    .clone()
                    .ok_or_else(|| anyhow!("TS-over-RTP muxer is not configured"))?,
            )
        } else {
            None
        };
        if !request.passive && request.dst_port == 0 {
            bail!("dst_port must be greater than zero");
        }
        if request.passive && request.is_udp {
            bail!("passive UDP RTP sending is not implemented; use TCP passive mode");
        }
        let source = self
            .source_manager
            .get(&request.vhost, &request.app, &request.stream)
            .ok_or_else(|| anyhow!("media source not found"))?;
        let key = Self::key(&request);
        if self.active.contains_key(&key) {
            bail!("RTP sender already exists for this stream and SSRC");
        }

        let (transport, destination) = if request.passive {
            let listener = TcpListener::bind(SocketAddr::new(
                IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                request.src_port,
            ))
            .await
            .context("failed to bind passive RTP TCP listener")?;
            (
                RtpTransport::TcpPassive {
                    listener,
                    stream: None,
                },
                None,
            )
        } else {
            let destination =
                tokio::net::lookup_host((request.dst_host.as_str(), request.dst_port))
                    .await
                    .context("failed to resolve RTP destination")?
                    .next()
                    .ok_or_else(|| anyhow!("RTP destination resolved to no address"))?;
            let transport = if request.is_udp {
                let bind_ip = match destination.ip() {
                    IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                };
                let socket = bind_udp_socket(
                    SocketAddr::new(bind_ip, request.src_port),
                    request.ssrc_multi_send,
                )?;
                socket
                    .connect(destination)
                    .await
                    .context("failed to connect RTP UDP socket")?;
                RtpTransport::Udp(socket)
            } else {
                let stream = connect_tcp(destination, request.src_port).await?;
                RtpTransport::TcpActive {
                    stream: Some(stream),
                    destination,
                    src_port: request.src_port,
                }
            };
            (transport, Some(destination))
        };
        let local_port = transport.local_port()?;

        let info = source.info.read().await;
        let preferred_frame_type = if request.only_audio {
            Some(FrameType::Audio)
        } else if info.tracks.iter().any(|track| {
            matches!(track, TrackInfo::Video(video) if matches!(video.codec, CodecId::H264 | CodecId::H265))
        }) {
            Some(FrameType::Video)
        } else if info
            .tracks
            .iter()
            .any(|track| matches!(track, TrackInfo::Audio(_)))
        {
            Some(FrameType::Audio)
        } else {
            None
        };
        let audio_clock = info
            .tracks
            .iter()
            .find_map(|track| match track {
                TrackInfo::Audio(audio) => Some(audio.sample_rate.max(1)),
                _ => None,
            })
            .unwrap_or(8_000);
        let tracks = info.tracks.clone();
        drop(info);

        let token = Uuid::new_v4();
        let stop = Arc::new(Notify::new());
        let entry = Arc::new(RtpSenderEntry {
            token,
            request: request.clone(),
            destination,
            local_port,
            stop: stop.clone(),
            stopped: AtomicBool::new(false),
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        self.active.insert(key.clone(), entry.clone());

        let active = self.active.clone();
        tokio::spawn(async move {
            let mut transport = transport;
            let session_id = format!("rtp-send:{key}");
            source.add_subscriber(session_id.clone()).await;
            let mut receiver = source.subscribe();
            let cached = source.get_latest_gop_frames().await;
            let packetizer = matches!(entry.request.data_type, RtpDataType::Ps | RtpDataType::Ts)
                .then(|| {
                    let packetizer =
                        RtpPacketizer::new(entry.request.ssrc, entry.request.payload_type, 90_000);
                    if matches!(entry.request.data_type, RtpDataType::Ts) {
                        packetizer.with_payload_mtu(188 * 7)
                    } else {
                        packetizer
                    }
                });
            let ps_muxer =
                matches!(entry.request.data_type, RtpDataType::Ps).then(|| PsMuxer::new(&tracks));
            let external_muxer = ts_muxer_factory.map(|factory| factory());
            let mut state = RtpSendState {
                preferred_frame_type,
                audio_clock,
                selected_track: None,
                packetizer,
                ps_muxer,
                external_muxer,
            };

            for frame in cached {
                if send_frame(&mut transport, &entry, &frame, &mut state)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            loop {
                if entry.stopped.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    _ = stop.notified() => break,
                    received = receiver.recv() => match received {
                        Ok(frame) => {
                            if send_frame(&mut transport, &entry, &frame, &mut state).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            source.remove_subscriber(&session_id).await;
            if active
                .get(&key)
                .is_some_and(|current| current.token == token)
            {
                active.remove(&key);
            }
        });
        Ok(local_port)
    }

    pub fn stop(&self, vhost: &str, app: &str, stream: &str, ssrc: Option<u32>) -> usize {
        let keys: Vec<String> = self
            .active
            .iter()
            .filter(|entry| {
                let request = &entry.request;
                request.vhost == vhost
                    && request.app == app
                    && request.stream == stream
                    && ssrc.is_none_or(|value| request.ssrc == value)
            })
            .map(|entry| entry.key().clone())
            .collect();
        for key in &keys {
            if let Some((_, entry)) = self.active.remove(key) {
                entry.stopped.store(true, Ordering::Release);
                entry.stop.notify_waiters();
            }
        }
        keys.len()
    }

    pub fn list(&self) -> Vec<RtpSenderInfo> {
        self.active
            .iter()
            .map(|entry| RtpSenderInfo {
                vhost: entry.request.vhost.clone(),
                app: entry.request.app.clone(),
                stream: entry.request.stream.clone(),
                ssrc: entry.request.ssrc,
                destination: entry.destination,
                local_port: entry.local_port,
                packets: entry.packets.load(Ordering::Relaxed),
                bytes: entry.bytes.load(Ordering::Relaxed),
            })
            .collect()
    }
}

struct RtpSendState {
    preferred_frame_type: Option<FrameType>,
    audio_clock: u32,
    selected_track: Option<u32>,
    packetizer: Option<RtpPacketizer>,
    ps_muxer: Option<PsMuxer>,
    external_muxer: Option<Box<dyn RtpFrameMuxer>>,
}

async fn send_frame(
    transport: &mut RtpTransport,
    entry: &RtpSenderEntry,
    frame: &MediaFrame,
    state: &mut RtpSendState,
) -> Result<()> {
    if let Some(muxer) = &mut state.external_muxer {
        let payload = muxer.mux_frame(frame)?;
        let packets = state
            .packetizer
            .as_mut()
            .expect("container packetizer is initialized with external muxer")
            .packetize_payload(&payload, frame.timestamp);
        return send_packets(transport, entry, packets).await;
    }

    if let Some(muxer) = &mut state.ps_muxer {
        let supported = matches!(
            frame.codec,
            CodecId::H264
                | CodecId::H265
                | CodecId::AAC
                | CodecId::G711A
                | CodecId::G711U
                | CodecId::Mp3
                | CodecId::MP2A
        );
        if !supported || frame.frame_type == FrameType::Metadata {
            return Ok(());
        }
        let payload = muxer.mux_frame(frame)?;
        let packets = state
            .packetizer
            .as_mut()
            .expect("PS packetizer is initialized with PS muxer")
            .packetize_payload(&payload, frame.timestamp);
        return send_packets(transport, entry, packets).await;
    }

    let supported = match frame.frame_type {
        FrameType::Video => matches!(frame.codec, CodecId::H264 | CodecId::H265),
        FrameType::Audio => matches!(
            frame.codec,
            CodecId::AAC
                | CodecId::Opus
                | CodecId::G711A
                | CodecId::G711U
                | CodecId::Mp3
                | CodecId::MP2A
                | CodecId::L16
        ),
        FrameType::Metadata => false,
    };
    let eligible = supported
        && state
            .preferred_frame_type
            .is_none_or(|frame_type| frame.frame_type == frame_type);
    if !eligible
        || state
            .selected_track
            .is_some_and(|track| track != frame.track_id)
    {
        return Ok(());
    }
    if state.selected_track.is_none() {
        state.selected_track = Some(frame.track_id);
        let clock_rate = if frame.frame_type == FrameType::Video {
            90_000
        } else if frame.codec == CodecId::Opus {
            48_000
        } else {
            state.audio_clock
        };
        state.packetizer = Some(RtpPacketizer::new(
            entry.request.ssrc,
            entry.request.payload_type,
            clock_rate,
        ));
    }
    let packets = state
        .packetizer
        .as_mut()
        .expect("packetizer is initialized with selected track")
        .packetize(frame)?;
    send_packets(transport, entry, packets).await
}

async fn send_packets(
    transport: &mut RtpTransport,
    entry: &RtpSenderEntry,
    packets: Vec<Vec<u8>>,
) -> Result<()> {
    for packet in packets {
        if entry.stopped.load(Ordering::Acquire) {
            bail!("RTP sender stopped");
        }
        tokio::select! {
            _ = entry.stop.notified() => bail!("RTP sender stopped"),
            result = transport.send(&packet) => result?,
        }
        entry.packets.fetch_add(1, Ordering::Relaxed);
        entry
            .bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_frame::{MediaFrame, VideoInfo};
    use bytes::Bytes;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn h264_frame(timestamp: u32, data: Vec<u8>) -> MediaFrame {
        MediaFrame::new_video(
            0,
            CodecId::H264,
            timestamp,
            timestamp as u64,
            timestamp as u64,
            Bytes::from(data),
            true,
        )
        .with_payload_format(PayloadFormat::AnnexB)
    }

    #[test]
    fn h264_single_nalu_has_expected_header() {
        let mut packetizer = RtpPacketizer::new(0x11223344, 98, 90_000).with_sequence(7);
        let packets = packetizer
            .packetize(&h264_frame(1000, vec![0, 0, 0, 1, 0x65, 1, 2, 3]))
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][1], 0x80 | 98);
        assert_eq!(&packets[0][2..4], &7u16.to_be_bytes());
        assert_eq!(&packets[0][4..8], &90_000u32.to_be_bytes());
        assert_eq!(&packets[0][8..12], &0x11223344u32.to_be_bytes());
        assert_eq!(&packets[0][12..], &[0x65, 1, 2, 3]);
    }

    #[test]
    fn h264_large_nalu_uses_fu_a_and_marks_last_packet() {
        let mut data = vec![0, 0, 0, 1, 0x65];
        data.extend(std::iter::repeat_n(0x55, 3_000));
        let mut packetizer = RtpPacketizer::new(1, 96, 90_000);
        let packets = packetizer.packetize(&h264_frame(0, data)).unwrap();
        assert!(packets.len() > 1);
        assert_eq!(packets[0][12] & 0x1f, 28);
        assert_ne!(packets[0][13] & 0x80, 0);
        assert_eq!(packets[0][1] & 0x80, 0);
        assert_ne!(packets.last().unwrap()[13] & 0x40, 0);
        assert_ne!(packets.last().unwrap()[1] & 0x80, 0);
    }

    #[tokio::test]
    async fn udp_sender_streams_media_source_and_stops_by_ssrc() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = receiver.local_addr().unwrap();
        let source_manager = Arc::new(MediaSourceManager::default());
        let source = source_manager.get_or_create("__defaultVhost__", "live", "camera");
        source
            .update_tracks(vec![TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 1920,
                height: 1080,
                fps: 25.0,
                key_frame: true,
            })])
            .await;
        let manager = RtpSenderManager::new(source_manager);
        let local_port = manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".into(),
                app: "live".into(),
                stream: "camera".into(),
                ssrc: 1234,
                dst_host: destination.ip().to_string(),
                dst_port: destination.port(),
                src_port: 0,
                payload_type: 96,
                is_udp: true,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: false,
            })
            .await
            .unwrap();
        assert_ne!(local_port, 0);
        tokio::time::sleep(Duration::from_millis(20)).await;
        source
            .publish_and_cache(h264_frame(40, vec![0, 0, 0, 1, 0x65, 9, 8, 7]))
            .await;
        let mut packet = [0u8; 1500];
        let received = tokio::time::timeout(Duration::from_secs(1), receiver.recv(&mut packet))
            .await
            .unwrap()
            .unwrap();
        assert!(received > 12);
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 1234);
        assert_eq!(
            manager.stop("__defaultVhost__", "live", "camera", Some(1234)),
            1
        );
    }

    #[tokio::test]
    async fn udp_multi_send_reuses_source_port_for_the_same_ssrc() {
        let receiver_one = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver_two = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let source_manager = Arc::new(MediaSourceManager::default());
        let source = source_manager.get_or_create("__defaultVhost__", "live", "cascade");
        source
            .update_tracks(vec![TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 640,
                height: 360,
                fps: 25.0,
                key_frame: true,
            })])
            .await;
        let manager = RtpSenderManager::new(source_manager);
        let first_port = manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".into(),
                app: "live".into(),
                stream: "cascade".into(),
                ssrc: 7788,
                dst_host: "127.0.0.1".into(),
                dst_port: receiver_one.local_addr().unwrap().port(),
                src_port: 0,
                payload_type: 96,
                is_udp: true,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: true,
            })
            .await
            .unwrap();
        let second_port = manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".into(),
                app: "live".into(),
                stream: "cascade".into(),
                ssrc: 7788,
                dst_host: "127.0.0.1".into(),
                dst_port: receiver_two.local_addr().unwrap().port(),
                src_port: first_port,
                payload_type: 96,
                is_udp: true,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: true,
            })
            .await
            .unwrap();
        assert_eq!(second_port, first_port);
        tokio::time::sleep(Duration::from_millis(20)).await;
        source
            .publish_and_cache(h264_frame(40, vec![0, 0, 0, 1, 0x65, 7, 7, 8, 8]))
            .await;
        let mut one = [0u8; 1500];
        let mut two = [0u8; 1500];
        tokio::time::timeout(Duration::from_secs(1), receiver_one.recv(&mut one))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), receiver_two.recv(&mut two))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(u32::from_be_bytes(one[8..12].try_into().unwrap()), 7788);
        assert_eq!(u32::from_be_bytes(two[8..12].try_into().unwrap()), 7788);
        assert_eq!(
            manager.stop("__defaultVhost__", "live", "cascade", Some(7788)),
            2
        );
    }

    #[tokio::test]
    async fn tcp_sender_uses_rfc4571_length_prefix() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = listener.local_addr().unwrap();
        let source_manager = Arc::new(MediaSourceManager::default());
        let source = source_manager.get_or_create("__defaultVhost__", "live", "tcp-camera");
        source
            .update_tracks(vec![TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 640,
                height: 360,
                fps: 25.0,
                key_frame: true,
            })])
            .await;
        let manager = RtpSenderManager::new(source_manager);
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".into(),
                app: "live".into(),
                stream: "tcp-camera".into(),
                ssrc: 5678,
                dst_host: destination.ip().to_string(),
                dst_port: destination.port(),
                src_port: 0,
                payload_type: 96,
                is_udp: false,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: false,
            })
            .await
            .unwrap();
        let mut stream = accept.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        source
            .publish_and_cache(h264_frame(80, vec![0, 0, 0, 1, 0x65, 4, 5, 6]))
            .await;

        let mut length = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut length))
            .await
            .unwrap()
            .unwrap();
        let length = u16::from_be_bytes(length) as usize;
        let mut packet = vec![0u8; length];
        stream.read_exact(&mut packet).await.unwrap();
        assert!(length > 12);
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 5678);
        assert_eq!(
            manager.stop("__defaultVhost__", "live", "tcp-camera", Some(5678)),
            1
        );
    }

    #[tokio::test]
    async fn passive_tcp_sender_listens_and_streams_after_accept() {
        let source_manager = Arc::new(MediaSourceManager::default());
        let source = source_manager.get_or_create("__defaultVhost__", "live", "passive-camera");
        source
            .update_tracks(vec![TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 640,
                height: 360,
                fps: 25.0,
                key_frame: true,
            })])
            .await;
        let manager = RtpSenderManager::new(source_manager);
        let local_port = manager
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".into(),
                app: "live".into(),
                stream: "passive-camera".into(),
                ssrc: 2468,
                dst_host: String::new(),
                dst_port: 0,
                src_port: 0,
                payload_type: 96,
                is_udp: false,
                data_type: RtpDataType::Ps,
                only_audio: false,
                passive: true,
                ssrc_multi_send: false,
            })
            .await
            .unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        source
            .publish_and_cache(h264_frame(40, vec![0, 0, 0, 1, 0x65, 9, 8, 7]))
            .await;

        let mut length = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut length))
            .await
            .unwrap()
            .unwrap();
        let mut packet = vec![0u8; u16::from_be_bytes(length) as usize];
        stream.read_exact(&mut packet).await.unwrap();
        assert_eq!(&packet[12..16], &[0, 0, 1, 0xba]);
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 2468);

        stream.shutdown().await.unwrap();
        drop(stream);
        let mut reconnected = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        for timestamp in [80, 120, 160] {
            source
                .publish_and_cache(h264_frame(
                    timestamp,
                    vec![0, 0, 0, 1, 0x41, timestamp as u8],
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::timeout(Duration::from_secs(1), reconnected.read_exact(&mut length))
            .await
            .unwrap()
            .unwrap();
        let mut packet = vec![0u8; u16::from_be_bytes(length) as usize];
        reconnected.read_exact(&mut packet).await.unwrap();
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 2468);
        assert_eq!(
            manager.stop("__defaultVhost__", "live", "passive-camera", None),
            1
        );
    }
}
