//! SRT ingest server — listens for incoming SRT connections and
//! publishes the received stream to `MediaSourceManager`.

use crate::ffi;
use std::ffi::c_int;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};
use zlmediakit_core::media_frame::{CodecId, MediaFrame, PayloadFormat};
use zlmediakit_core::media_source::MediaSourceManager;

/// Configuration for the SRT server.
pub struct SrtServerConfig {
    pub addr: String,
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    pub source_manager: Arc<MediaSourceManager>,
}

/// An SRT ingest server. Binds a UDP port, listens for SRT connections,
/// and publishes incoming data to the MediaSourceManager.
pub struct SrtServer {
    config: SrtServerConfig,
    started: bool,
}

impl SrtServer {
    pub fn new(config: SrtServerConfig) -> Self {
        Self {
            config,
            started: false,
        }
    }

    /// Starts the SRT server. This will block the calling thread (use in
    /// a dedicated `tokio::task::spawn_blocking`).
    pub fn run_blocking(&mut self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        if self.started {
            anyhow::bail!("SRT server already started");
        }
        self.started = true;

        ffi::ensure_startup()?;

        let addr: SocketAddr = self.config.addr.parse()?;

        // Create listener socket
        let sock = unsafe { ffi::srt_create_socket() };
        if sock < 0 {
            anyhow::bail!("srt_create_socket failed: {}", ffi::last_error());
        }

        // Set socket options
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_TRANSTYPE, ffi::SRTT_LIVE)?;
        ffi::set_sockflag_int(
            sock,
            ffi::SRT_SOCKOPT_LATENCY,
            self.config.latency_ms as c_int,
        )?;
        ffi::set_sockflag_int(
            sock,
            ffi::SRT_SOCKOPT_RCVLATENCY,
            self.config.latency_ms as c_int,
        )?;
        ffi::set_sockflag_bool(sock, ffi::SRT_SOCKOPT_RCVSYN, false)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVBUF, 1024 * 1024)?;

        if let Some(ref pw) = self.config.passphrase {
            ffi::set_sockflag_str(sock, ffi::SRT_SOCKOPT_PASSPHRASE, pw)?;
            ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_PBKEYLEN, 16)?;
        }

        // Bind
        let sockaddr = ffi::socket_addr_to_sockaddr(&addr);
        let ret = unsafe { ffi::srt_bind(sock, sockaddr.as_ptr().cast(), sockaddr.len() as c_int) };
        if ret != ffi::SRT_SUCCESS {
            let err = ffi::last_error();
            unsafe { ffi::srt_close(sock) };
            anyhow::bail!("srt_bind({}) failed: {}", addr, err);
        }

        // Listen
        let ret = unsafe { ffi::srt_listen(sock, 5) };
        if ret != ffi::SRT_SUCCESS {
            let err = ffi::last_error();
            unsafe { ffi::srt_close(sock) };
            anyhow::bail!("srt_listen failed: {}", err);
        }

        info!("SRT server listening on {}", addr);

        let sm = self.config.source_manager.clone();

        // Accept loop
        loop {
            // Check stop signal
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let accept_fd =
                unsafe { ffi::srt_accept(sock, std::ptr::null_mut(), std::ptr::null_mut()) };

            if accept_fd < 0 {
                let state = unsafe { ffi::srt_getsockstate(sock) };
                if state == ffi::SRTS_BROKEN {
                    error!("SRT listener socket broken");
                    break;
                }
                // SRT_EASYNCRCV means no connection pending (non-blocking
                // accept) — yield quickly without sleeping.
                let err = unsafe {
                    let mut e = 0;
                    ffi::srt_getlasterror(&mut e);
                    e
                };
                if matches!(err, ffi::SRT_EASYNCFAIL | ffi::SRT_EASYNCRCV) {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                continue;
            }

            let streamid = ffi::get_streamid(accept_fd);
            if let Err(error) = ffi::set_sockflag_bool(accept_fd, ffi::SRT_SOCKOPT_RCVSYN, true)
                .and_then(|_| ffi::set_sockflag_int(accept_fd, ffi::SRT_SOCKOPT_RCVTIMEO, 200))
            {
                warn!("SRT accepted socket setup failed (fd={accept_fd}): {error}");
                unsafe { ffi::srt_close(accept_fd) };
                continue;
            }
            if let Some(ref sid) = streamid {
                info!("SRT client connected (fd={}, streamid={})", accept_fd, sid);
            } else {
                info!("SRT client connected (fd={}, no streamid)", accept_fd);
            }

            let sm2 = sm.clone();
            tokio::spawn(async move {
                handle_srt_connection(accept_fd, sm2, streamid).await;
            });
        }

        unsafe { ffi::srt_close(sock) };
        info!("SRT server stopped");
        Ok(())
    }
}

/// MPEG-TS constants.
const TS_PACKET_SIZE: usize = 188;
const TS_SYNC: u8 = 0x47;
const PID_PAT: u16 = 0x0000;
const PID_DEFAULT_AUDIO: u16 = 0x1100;

/// Holds TS stream state for a single SRT connection.
pub(crate) struct TsContext {
    /// PID of the video elementary stream (from PMT).
    video_pid: Option<u16>,
    /// Video codec declared by the PMT stream type.
    video_codec: Option<CodecId>,
    /// PID of the audio elementary stream (from PMT).
    audio_pid: Option<u16>,
    /// PMT PID discovered from PAT, used to route PSI packets to the PMT parser.
    pmt_pid: Option<u16>,
    /// Accumulated PES payload for the current video frame.
    video_accum: Vec<u8>,
    /// Accumulated PES payload for the current audio frame.
    audio_accum: Vec<u8>,
    video_pts: Option<u64>,
    video_dts: Option<u64>,
    audio_pts: Option<u64>,
    audio_dts: Option<u64>,
}

impl TsContext {
    pub(crate) fn new() -> Self {
        Self {
            video_pid: None,
            video_codec: None,
            audio_pid: None,
            pmt_pid: None,
            video_accum: Vec::new(),
            audio_accum: Vec::new(),
            video_pts: None,
            video_dts: None,
            audio_pts: None,
            audio_dts: None,
        }
    }
}

/// Handles a single SRT connection: reads TS data in a loop, demuxes
/// video and audio from MPEG-TS packets, and publishes them as
/// `MediaFrame`s.
async fn handle_srt_connection(fd: c_int, sm: Arc<MediaSourceManager>, streamid: Option<String>) {
    let (app, stream_name) = match &streamid {
        Some(sid) => ffi::parse_streamid(sid),
        None => ("live".to_string(), format!("srt_{}", fd)),
    };
    let source = sm.get_or_create("__defaultVhost__", &app, &stream_name);
    let mut ctx = TsContext::new();
    let mut timestamp: u32 = 0;
    let buf_cap = TS_PACKET_SIZE * 50;

    loop {
        let fd_copy = fd;
        let result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; buf_cap];
            unsafe {
                let n = ffi::srt_recvmsg(
                    fd_copy,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    buf.len() as c_int,
                );
                if n > 0 {
                    buf.truncate(n as usize);
                    Ok(buf)
                } else {
                    Err(n)
                }
            }
        })
        .await;

        match result {
            Ok(Ok(data)) => {
                if data.len() >= TS_PACKET_SIZE && data[0] == TS_SYNC {
                    demux_ts_packets(&data, &mut ctx, &source, &mut timestamp).await;
                } else if data.len() >= 4 {
                    // Not TS — fall back to raw frame detection.
                    publish_raw_frame(&data, &source, &mut timestamp).await;
                    timestamp = timestamp.wrapping_add(40);
                }
            }
            Ok(Err(0)) => {
                info!("SRT connection closed (fd={})", fd);
                break;
            }
            Ok(Err(n)) => {
                let code = ffi::last_error_code();
                if matches!(
                    code,
                    ffi::SRT_EASYNCFAIL | ffi::SRT_EASYNCRCV | ffi::SRT_ETIMEOUT
                ) {
                    continue;
                }
                let err = ffi::last_error();
                warn!("SRT recv error (fd={}): code={}, {}", fd, n, err);
                break;
            }
            Err(e) => {
                warn!("SRT spawn_blocking join error: {}", e);
                break;
            }
        }
    }

    flush_ts_context(&mut ctx, &source, &mut timestamp).await;

    unsafe { ffi::srt_close(fd) };
}

/// Iterates over 188-byte TS packets and publishes video/audio frames.
pub(crate) async fn demux_ts_packets(
    data: &[u8],
    ctx: &mut TsContext,
    source: &zlmediakit_core::media_source::MediaSource,
    timestamp: &mut u32,
) {
    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= data.len() {
        if data[offset] != TS_SYNC {
            break;
        }

        let pid = ((data[offset + 1] as u16 & 0x1F) << 8) | data[offset + 2] as u16;
        let payload_unit_start = (data[offset + 1] & 0x40) != 0;
        let adaptation = (data[offset + 3] >> 4) & 0x03;
        let mut payload_offset = offset + 4;

        // Skip adaptation field if present.
        if adaptation == 0x02 || adaptation == 0x03 {
            let adapt_len = data[offset + 4] as usize + 1;
            payload_offset += adapt_len;
        }

        if payload_offset + 4 > offset + TS_PACKET_SIZE {
            offset += TS_PACKET_SIZE;
            continue;
        }

        let payload = &data[payload_offset..offset + TS_PACKET_SIZE];

        // Parse PAT to find PMT PID, then PMT to discover audio/video PIDs.
        if pid == PID_PAT && payload_unit_start {
            parse_pat(payload, ctx);
        }
        // Check if this is a PMT packet (using PAT-discovered PID, or fallback heuristic).
        let is_pmt = ctx.pmt_pid.map_or(
            payload_unit_start && payload.len() > 3 && payload[1] == 0x02,
            |ppid| pid == ppid,
        );
        if is_pmt {
            parse_pmt(payload, ctx);
        }

        // Use default PIDs until PMT is parsed.
        let video_pid = ctx.video_pid.unwrap_or(0x0100);
        let audio_pid = ctx.audio_pid.unwrap_or(PID_DEFAULT_AUDIO);

        let is_video = pid == video_pid;
        let is_audio = pid == audio_pid;

        if is_video && !payload.is_empty() {
            if let Some(pes) = extract_pes_payload(payload, payload_unit_start) {
                if payload_unit_start {
                    if !ctx.video_accum.is_empty() {
                        publish_video(
                            &ctx.video_accum,
                            source,
                            ctx.video_codec,
                            ctx.video_pts,
                            ctx.video_dts,
                            timestamp,
                        )
                        .await;
                        ctx.video_accum.clear();
                    }
                    ctx.video_pts = pes.pts;
                    ctx.video_dts = pes.dts.or(pes.pts);
                }
                ctx.video_accum.extend_from_slice(pes.data);
            }
        } else if is_audio && !payload.is_empty() {
            if let Some(pes) = extract_pes_payload(payload, payload_unit_start) {
                if payload_unit_start && !ctx.audio_accum.is_empty() {
                    publish_audio(
                        &ctx.audio_accum,
                        source,
                        ctx.audio_pts,
                        ctx.audio_dts,
                        timestamp,
                    )
                    .await;
                    ctx.audio_accum.clear();
                }
                if payload_unit_start {
                    ctx.audio_pts = pes.pts;
                    ctx.audio_dts = pes.dts.or(pes.pts);
                }
                ctx.audio_accum.extend_from_slice(pes.data);
            }
        }

        offset += TS_PACKET_SIZE;
    }

    // A PES may span multiple SRT receive calls. It is flushed when the next
    // payload-unit-start arrives, or when the connection closes.
}

async fn flush_ts_context(
    ctx: &mut TsContext,
    source: &zlmediakit_core::media_source::MediaSource,
    timestamp: &mut u32,
) {
    if !ctx.video_accum.is_empty() {
        publish_video(
            &ctx.video_accum,
            source,
            ctx.video_codec,
            ctx.video_pts,
            ctx.video_dts,
            timestamp,
        )
        .await;
        ctx.video_accum.clear();
    }
    if !ctx.audio_accum.is_empty() {
        publish_audio(
            &ctx.audio_accum,
            source,
            ctx.audio_pts,
            ctx.audio_dts,
            timestamp,
        )
        .await;
        ctx.audio_accum.clear();
    }
}

/// Incremental MPEG-TS demuxer shared by SRT, HTTP-TS and HLS pull inputs.
/// It preserves partial 188-byte packets across network reads and publishes
/// normalized media frames into the supplied media source.
pub struct TsStreamDemuxer {
    pending: Vec<u8>,
    context: TsContext,
    timestamp: u32,
}

impl TsStreamDemuxer {
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(TS_PACKET_SIZE * 16),
            context: TsContext::new(),
            timestamp: 0,
        }
    }

    pub async fn feed(&mut self, data: &[u8], source: &zlmediakit_core::media_source::MediaSource) {
        self.pending.extend_from_slice(data);
        while self.pending.len() >= TS_PACKET_SIZE {
            if self.pending[0] != TS_SYNC {
                if let Some(offset) = self.pending.iter().position(|byte| *byte == TS_SYNC) {
                    self.pending.drain(..offset);
                } else {
                    self.pending.clear();
                    return;
                }
                if self.pending.len() < TS_PACKET_SIZE {
                    return;
                }
            }
            let packet: Vec<u8> = self.pending.drain(..TS_PACKET_SIZE).collect();
            demux_ts_packets(&packet, &mut self.context, source, &mut self.timestamp).await;
        }
    }

    pub async fn flush(&mut self, source: &zlmediakit_core::media_source::MediaSource) {
        flush_ts_context(&mut self.context, source, &mut self.timestamp).await;
        self.pending.clear();
    }
}

impl Default for TsStreamDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses PAT to find PMT PID.
fn parse_pat(payload: &[u8], ctx: &mut TsContext) {
    // PAT program loop starts after table_id, section_length,
    // transport_stream_id, version/current, section_number and
    // last_section_number (8 bytes after the pointer field).
    if payload.len() < 12 {
        return;
    }
    let pointer = payload[0] as usize;
    let pat_start = 1 + pointer;
    if pat_start + 4 > payload.len() {
        return;
    }
    let program_offset = pat_start + 8;
    if program_offset + 4 > payload.len() {
        return;
    }
    let program_number = u16::from_be_bytes([payload[program_offset], payload[program_offset + 1]]);
    if program_number == 0x0000 {
        // NIT — skip.
        return;
    }
    // Store PMT PID for parsing later (encoded into program_map_PID).
    let pmt_pid =
        ((payload[program_offset + 2] as u16 & 0x1F) << 8) | payload[program_offset + 3] as u16;
    ctx.pmt_pid = Some(pmt_pid);
    ctx.video_pid = ctx.video_pid.or(Some(0x0100));
    ctx.audio_pid = ctx.audio_pid.or(Some(PID_DEFAULT_AUDIO));
}

/// Parses PMT to discover video and audio PIDs.
fn parse_pmt(payload: &[u8], ctx: &mut TsContext) {
    // PMT: pointer (1) + table_id (1) + section_length (2)
    if payload.len() < 12 {
        return;
    }
    let pointer = payload[0] as usize;
    let pmt_start = 1 + pointer;
    if pmt_start + 7 > payload.len() {
        return;
    }
    // Skip: table_id(1), section_length(2), program_number(2), version/current(1),
    // section_number(1), last_section_number(1) = 8 bytes
    let pcr_pid_offset = pmt_start + 8;
    if pcr_pid_offset + 5 > payload.len() {
        return;
    }
    // Skip PCR_PID (2) + program_info_length (2) = 4 bytes
    let info_len =
        ((payload[pcr_pid_offset + 2] as u16 & 0x0F) << 8) | payload[pcr_pid_offset + 3] as u16;
    let es_start = pcr_pid_offset + 4 + info_len as usize;

    let mut pos = es_start;
    while pos + 5 <= payload.len() {
        let stream_type = payload[pos];
        let pid = ((payload[pos + 1] as u16 & 0x1F) << 8) | payload[pos + 2] as u16;
        // es_info_length (2)
        let es_info = ((payload[pos + 3] as u16 & 0x0F) << 8) | payload[pos + 4] as u16;
        match stream_type {
            0x1B => {
                ctx.video_pid = Some(pid);
                ctx.video_codec = Some(CodecId::H264);
            }
            0x24 => {
                ctx.video_pid = Some(pid);
                ctx.video_codec = Some(CodecId::H265);
            }
            0x0F | 0x11 => ctx.audio_pid = Some(pid), // AAC / AAC-latm
            _ => {}
        }
        pos += 5 + es_info as usize;
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PesPayload<'a> {
    data: &'a [u8],
    pts: Option<u64>,
    dts: Option<u64>,
}

/// Extracts a PES elementary-stream payload and its 90 kHz timestamps.
fn extract_pes_payload(payload: &[u8], unit_start: bool) -> Option<PesPayload<'_>> {
    if !unit_start {
        return Some(PesPayload {
            data: payload,
            pts: None,
            dts: None,
        });
    }
    if payload.len() < 9 {
        return None;
    }
    // PES header: 0x00 0x00 0x01 stream_id
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return Some(PesPayload {
            data: payload,
            pts: None,
            dts: None,
        });
    }
    let pts_dts_flags = (payload[7] >> 6) & 0x03;
    let header_len = payload[8] as usize;
    let data_start = 9 + header_len;
    if data_start > payload.len() {
        return None;
    }
    let pts = if matches!(pts_dts_flags, 0x02 | 0x03) {
        parse_pes_timestamp(payload.get(9..14)?)
    } else {
        None
    };
    let dts = if pts_dts_flags == 0x03 {
        parse_pes_timestamp(payload.get(14..19)?)
    } else {
        None
    };
    Some(PesPayload {
        data: &payload[data_start..],
        pts,
        dts,
    })
}

fn parse_pes_timestamp(data: &[u8]) -> Option<u64> {
    if data.len() < 5 {
        return None;
    }
    Some(
        (((data[0] as u64 >> 1) & 0x07) << 30)
            | ((data[1] as u64) << 22)
            | (((data[2] as u64 >> 1) & 0x7f) << 15)
            | ((data[3] as u64) << 7)
            | ((data[4] as u64 >> 1) & 0x7f),
    )
}

/// Publishes accumulated video data as a MediaFrame.
async fn publish_video(
    data: &[u8],
    source: &zlmediakit_core::media_source::MediaSource,
    declared_codec: Option<CodecId>,
    pts: Option<u64>,
    dts: Option<u64>,
    fallback_timestamp: &mut u32,
) {
    if data.len() < 4 {
        return;
    }
    let codec = declared_codec.unwrap_or_else(|| detect_codec(data));
    let key_frame = is_keyframe(data, codec);
    let (timestamp, pts_ms, dts_ms) = media_timestamps(pts, dts, fallback_timestamp);
    let frame = MediaFrame::new_video(
        0,
        codec,
        timestamp,
        pts_ms,
        dts_ms,
        bytes::Bytes::copy_from_slice(data),
        key_frame,
    )
    .with_payload_format(PayloadFormat::AnnexB);
    source.publish_and_cache(frame).await;
}

/// Publishes accumulated audio data as a MediaFrame.
async fn publish_audio(
    data: &[u8],
    source: &zlmediakit_core::media_source::MediaSource,
    pts: Option<u64>,
    dts: Option<u64>,
    fallback_timestamp: &mut u32,
) {
    if data.len() < 7 {
        return;
    }
    // Check for AAC ADTS sync word (0xFFF).
    if data[0] == 0xFF && (data[1] & 0xF0) == 0xF0 {
        let (timestamp, pts_ms, dts_ms) = media_timestamps(pts, dts, fallback_timestamp);
        let frame = MediaFrame::new_audio(
            0,
            zlmediakit_core::media_frame::CodecId::AAC,
            timestamp,
            pts_ms,
            dts_ms,
            bytes::Bytes::copy_from_slice(data),
        )
        .with_payload_format(PayloadFormat::Adts);
        source.publish_and_cache(frame).await;
    }
}

fn media_timestamps(
    pts: Option<u64>,
    dts: Option<u64>,
    fallback_timestamp: &mut u32,
) -> (u32, u64, u64) {
    let pts_ms = pts.map(|value| value.saturating_mul(1000) / 90_000);
    let dts_ms = dts.or(pts).map(|value| value.saturating_mul(1000) / 90_000);
    match (pts_ms, dts_ms) {
        (Some(pts_ms), Some(dts_ms)) => {
            *fallback_timestamp = dts_ms as u32;
            (dts_ms as u32, pts_ms, dts_ms)
        }
        _ => {
            let timestamp = *fallback_timestamp;
            *fallback_timestamp = (*fallback_timestamp).wrapping_add(40);
            (timestamp, timestamp as u64, timestamp as u64)
        }
    }
}

/// Fallback: publish raw data as a video frame (pre-TS behaviour).
async fn publish_raw_frame(
    data: &[u8],
    source: &zlmediakit_core::media_source::MediaSource,
    timestamp: &mut u32,
) {
    if data.len() >= 5 {
        let codec = detect_codec(data);
        let key_frame = is_keyframe(data, codec);
        let frame = MediaFrame::new_video(
            0,
            codec,
            *timestamp,
            *timestamp as u64,
            *timestamp as u64,
            bytes::Bytes::copy_from_slice(data),
            key_frame,
        )
        .with_payload_format(PayloadFormat::AnnexB);
        source.publish_and_cache(frame).await;
    }
}

fn detect_codec(data: &[u8]) -> CodecId {
    for w in data.windows(4) {
        if w == [0x00, 0x00, 0x00, 0x01] {
            let offset = w.as_ptr() as usize - data.as_ptr() as usize;
            if let Some(&b) = data.get(offset + 4) {
                let nal5 = b & 0x1F;
                let nal6 = (b >> 1) & 0x3F;
                // VPS (32), SPS (33), PPS (34) are H.265-specific
                if matches!(nal6, 32..=34) {
                    return CodecId::H265;
                }
                // H.264 IRAP/SPS/PPS types use 5-bit coding
                if matches!(nal5, 5 | 7 | 8) {
                    return CodecId::H264;
                }
                // H.265 IRAP types (16..=21) use 6-bit coding
                if matches!(nal6, 16..=21) {
                    return CodecId::H265;
                }
                // Default: 5-bit NAL type > 0 suggests H.264
                if nal5 > 0 {
                    return CodecId::H264;
                }
            }
            return CodecId::H264;
        }
    }
    for w in data.windows(3) {
        if w == [0x00, 0x00, 0x01] {
            let offset = w.as_ptr() as usize - data.as_ptr() as usize;
            if let Some(&b) = data.get(offset + 3) {
                let nal6 = (b >> 1) & 0x3F;
                let nal5 = b & 0x1F;
                if matches!(nal6, 32..=34) {
                    return CodecId::H265;
                }
                if matches!(nal5, 5 | 7 | 8) {
                    return CodecId::H264;
                }
                if matches!(nal6, 16..=21) {
                    return CodecId::H265;
                }
                if nal5 > 0 {
                    return CodecId::H264;
                }
            }
            return CodecId::H264;
        }
    }
    if !data.is_empty() && data[0] == 0x47 {
        return CodecId::Unknown(0);
    }
    CodecId::Unknown(0)
}

fn is_keyframe(data: &[u8], codec: CodecId) -> bool {
    match codec {
        CodecId::H264 => {
            for w in data.windows(5) {
                if (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1)
                {
                    let nal_type = if w[2] == 1 { w[3] & 0x1F } else { w[4] & 0x1F };
                    return nal_type == 5 || nal_type == 7;
                }
            }
            false
        }
        CodecId::H265 => {
            for w in data.windows(4) {
                if (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1)
                {
                    let b = w[3]; // byte after start code
                    let nal_type = (b >> 1) & 0x3F;
                    return (16..=21).contains(&nal_type);
                }
            }
            false
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_timestamp(prefix: u8, value: u64) -> [u8; 5] {
        [
            (prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1,
            (value >> 22) as u8,
            (((value >> 15) as u8 & 0x7f) << 1) | 1,
            (value >> 7) as u8,
            ((value as u8 & 0x7f) << 1) | 1,
        ]
    }

    #[test]
    fn extracts_pes_payload_and_pts_from_optional_header() {
        let pts = 90_000u64;
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5];
        pes.extend_from_slice(&encode_timestamp(2, pts));
        pes.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);

        let parsed = extract_pes_payload(&pes, true).unwrap();
        assert_eq!(parsed.pts, Some(pts));
        assert_eq!(parsed.dts, None);
        assert_eq!(parsed.data, &[0, 0, 0, 1, 0x65, 0x88]);
    }

    #[test]
    fn extracts_distinct_pts_and_dts() {
        let pts = 180_000u64;
        let dts = 176_400u64;
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0xc0, 10];
        pes.extend_from_slice(&encode_timestamp(3, pts));
        pes.extend_from_slice(&encode_timestamp(1, dts));
        pes.push(0xaa);

        let parsed = extract_pes_payload(&pes, true).unwrap();
        assert_eq!(parsed.pts, Some(pts));
        assert_eq!(parsed.dts, Some(dts));
        assert_eq!(parsed.data, &[0xaa]);
    }

    #[test]
    fn converts_pes_clock_to_media_milliseconds() {
        let mut fallback = 0;
        assert_eq!(
            media_timestamps(Some(180_000), Some(176_400), &mut fallback),
            (1960, 2000, 1960)
        );
        assert_eq!(fallback, 1960);
    }
}
