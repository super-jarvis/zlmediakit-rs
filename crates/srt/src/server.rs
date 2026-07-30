//! SRT ingest server — listens for incoming SRT connections and
//! publishes the received stream to `MediaSourceManager`.

use crate::ffi;
use std::ffi::c_int;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
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

        // Initialize SRT
        let ret = unsafe { ffi::srt_startup() };
        if ret != 0 {
            anyhow::bail!("srt_startup failed: {}", ffi::last_error());
        }

        let addr: SocketAddr = self.config.addr.parse()?;

        // Create listener socket
        let sock = unsafe { ffi::srt_create_socket() };
        if sock < 0 {
            anyhow::bail!("srt_create_socket failed: {}", ffi::last_error());
        }

        // Set socket options
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_TRANSTYPE, ffi::SRTT_LIVE)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_LATENCY, self.config.latency_ms as c_int)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVLATENCY, self.config.latency_ms as c_int)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVSYN, 0)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVBUF, 1024 * 1024)?;

        if let Some(ref pw) = self.config.passphrase {
            ffi::set_sockflag_str(sock, ffi::SRT_SOCKOPT_PASSPHRASE, pw)?;
            ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_PBKEYLEN, 16)?;
        }

        // Bind
        let (sockaddr, sockaddr_len) = ffi::socket_addr_to_sockaddr(&addr)?;
        let ret = unsafe {
            ffi::srt_bind(
                sock,
                &sockaddr as *const libc::sockaddr_in as *const libc::sockaddr,
                sockaddr_len as c_int,
            )
        };
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

            let accept_fd = unsafe { ffi::srt_accept(sock, std::ptr::null_mut(), std::ptr::null_mut()) };

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
                if err == ffi::SRT_EASYNCRCV {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                continue;
            }

            let streamid = ffi::get_streamid(accept_fd);
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
        unsafe { ffi::srt_cleanup() };
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
struct TsContext {
    /// PID of the video elementary stream (from PMT).
    video_pid: Option<u16>,
    /// PID of the audio elementary stream (from PMT).
    audio_pid: Option<u16>,
    /// PMT PID discovered from PAT, used to route PSI packets to the PMT parser.
    pmt_pid: Option<u16>,
    /// Accumulated PES payload for the current video frame.
    video_accum: Vec<u8>,
    /// Accumulated PES payload for the current audio frame.
    audio_accum: Vec<u8>,
}

impl TsContext {
    fn new() -> Self {
        Self {
            video_pid: None,
            audio_pid: None,
            pmt_pid: None,
            video_accum: Vec::new(),
            audio_accum: Vec::new(),
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
                let n = ffi::srt_recv(
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
                }
                timestamp += 40;
            }
            Ok(Err(0)) => {
                info!("SRT connection closed (fd={})", fd);
                break;
            }
            Ok(Err(n)) => {
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

    unsafe { ffi::srt_close(fd) };
}

/// Iterates over 188-byte TS packets and publishes video/audio frames.
async fn demux_ts_packets(
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
            let pes_payload = extract_pes_payload(payload, payload_unit_start);
            if let Some(p) = pes_payload {
                if payload_unit_start {
                    if !ctx.video_accum.is_empty() {
                        publish_video(&ctx.video_accum, source, timestamp).await;
                        ctx.video_accum.clear();
                    }
                    ctx.video_accum.extend_from_slice(p);
                } else {
                    ctx.video_accum.extend_from_slice(p);
                }
            }
        } else if is_audio && !payload.is_empty() {
            let pes_payload = extract_pes_payload(payload, payload_unit_start);
            if let Some(p) = pes_payload {
                if payload_unit_start && !ctx.audio_accum.is_empty() {
                    publish_audio(&ctx.audio_accum, source, timestamp).await;
                    ctx.audio_accum.clear();
                }
                ctx.audio_accum.extend_from_slice(p);
            }
        }

        offset += TS_PACKET_SIZE;
    }

    // Flush remaining accumulated data.
    if !ctx.video_accum.is_empty() {
        publish_video(&ctx.video_accum, source, timestamp).await;
        ctx.video_accum.clear();
    }
    if !ctx.audio_accum.is_empty() {
        publish_audio(&ctx.audio_accum, source, timestamp).await;
        ctx.audio_accum.clear();
    }
}

/// Parses PAT to find PMT PID.
fn parse_pat(payload: &[u8], ctx: &mut TsContext) {
    // PAT starts after pointer (1 byte) + table_id (1) + section_length (2).
    if payload.len() < 8 {
        return;
    }
    let pointer = payload[0] as usize;
    let pat_start = 1 + pointer;
    if pat_start + 4 > payload.len() {
        return;
    }
    // Skip table_id (1), section_syntax_indicator + reserved + section_length (2).
    let program_offset = pat_start + 3;
    if program_offset + 4 > payload.len() {
        return;
    }
    let program_number = u16::from_be_bytes([payload[program_offset], payload[program_offset + 1]]);
    if program_number == 0x0000 {
        // NIT — skip.
        return;
    }
    // Store PMT PID for parsing later (encoded into program_map_PID).
    let pmt_pid = ((payload[program_offset + 2] as u16 & 0x1F) << 8) | payload[program_offset + 3] as u16;
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
    let info_len = ((payload[pcr_pid_offset + 3] as u16 & 0x0F) << 8) | payload[pcr_pid_offset + 4] as u16;
    let es_start = pcr_pid_offset + 5 + info_len as usize;

    let mut pos = es_start;
    while pos + 5 <= payload.len() {
        let stream_type = payload[pos];
        let pid = ((payload[pos + 1] as u16 & 0x1F) << 8) | payload[pos + 2] as u16;
        // es_info_length (2)
        let es_info = ((payload[pos + 3] as u16 & 0x0F) << 8) | payload[pos + 4] as u16;
        match stream_type {
            0x1B | 0x24 => ctx.video_pid = Some(pid),  // H.264 / H.265
            0x0F | 0x11 => ctx.audio_pid = Some(pid),  // AAC / AAC-latm
            _ => {}
        }
        pos += 5 + es_info as usize;
    }
}

/// Extracts PES packet payload from a TS packet payload.
fn extract_pes_payload<'a>(payload: &'a [u8], unit_start: bool) -> Option<&'a [u8]> {
    if !unit_start {
        return Some(payload);
    }
    if payload.len() < 6 {
        return None;
    }
    // PES header: 0x00 0x00 0x01 stream_id
    if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
        return Some(payload);
    }
    let data_start = 6 + payload[4] as usize;
    if data_start > payload.len() {
        return None;
    }
    Some(&payload[data_start..])
}

/// Publishes accumulated video data as a MediaFrame.
async fn publish_video(data: &[u8], source: &zlmediakit_core::media_source::MediaSource, timestamp: &mut u32) {
    if data.len() < 4 {
        return;
    }
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
    );
    source.publish_and_cache(frame).await;
}

/// Publishes accumulated audio data as a MediaFrame.
async fn publish_audio(data: &[u8], source: &zlmediakit_core::media_source::MediaSource, timestamp: &mut u32) {
    if data.len() < 7 {
        return;
    }
    // Check for AAC ADTS sync word (0xFFF).
    if data[0] == 0xFF && (data[1] & 0xF0) == 0xF0 {
        let frame = MediaFrame::new_audio(
            0,
            zlmediakit_core::media_frame::CodecId::AAC,
            *timestamp,
            *timestamp as u64,
            *timestamp as u64,
            bytes::Bytes::copy_from_slice(data),
        );
        source.publish_and_cache(frame).await;
    }
}

/// Fallback: publish raw data as a video frame (pre-TS behaviour).
async fn publish_raw_frame(data: &[u8], source: &zlmediakit_core::media_source::MediaSource, timestamp: &mut u32) {
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
        );
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
                if matches!(nal6, 32..=34) { return CodecId::H265; }
                if matches!(nal5, 5 | 7 | 8) { return CodecId::H264; }
                if matches!(nal6, 16..=21) { return CodecId::H265; }
                if nal5 > 0 { return CodecId::H264; }
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
                    let b = if w[2] == 1 { w[3] } else { w[3] }; // byte after start code
                    let nal_type = (b >> 1) & 0x3F;
                    return (16..=21).contains(&nal_type);
                }
            }
            false
        }
        _ => true,
    }
}
