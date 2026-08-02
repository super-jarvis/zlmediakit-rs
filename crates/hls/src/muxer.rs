use bytes::{BufMut, BytesMut};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame, PayloadFormat};
use zlmediakit_core::media_source::MediaSource;
use zlmediakit_core::{
    aac_audio_specific_config, aac_raw_payload, video_decoder_config, video_sample_annex_b,
};

const TS_PACKET_SIZE: usize = 188;
const PAT_PID: u16 = 0;
const PMT_PID: u16 = 4096;
pub(crate) const VIDEO_PID: u16 = 256;
pub(crate) const AUDIO_PID: u16 = 257;

pub struct HlsSession {
    pub source: Arc<MediaSource>,
    pub segments: Arc<RwLock<VecDeque<HlsSegment>>>,
}

pub struct HlsSegment {
    pub index: u32,
    pub duration: f64,
    pub data: Vec<u8>,
}

pub(crate) struct TsWriter {
    continuity_video: u8,
    continuity_audio: u8,
    continuity_pat: u8,
    continuity_pmt: u8,
    pcr: u64,
}

impl TsWriter {
    pub(crate) fn new() -> Self {
        Self {
            continuity_video: 0,
            continuity_audio: 0,
            continuity_pat: 0,
            continuity_pmt: 0,
            pcr: 0,
        }
    }

    pub(crate) fn write_pat_pmt(&mut self, out: &mut Vec<u8>, video_stream_type: u8) {
        let mut pat = BytesMut::with_capacity(20);
        pat.put_u8(0x00); // table_id
        pat.put_u8(0xB0);
        pat.put_u8(0x0D); // section_length: 9 data + 4 CRC32
        pat.put_u16(0x0001);
        pat.put_u8(0xC1);
        pat.put_u8(0x00);
        pat.put_u8(0x00);
        pat.put_u16(0x0001);
        pat.put_u16(0xE000 | PMT_PID);
        let crc = crc32(pat.as_ref());
        pat.put_u32(crc);
        let mut pat_payload = BytesMut::with_capacity(20);
        pat_payload.put_u8(0x00); // pointer_field
        pat_payload.extend_from_slice(&pat);
        write_ts_packet(out, PAT_PID, &pat_payload, true, &mut self.continuity_pat);

        let mut pmt = BytesMut::with_capacity(32);
        pmt.put_u8(0x02); // table_id
        pmt.put_u8(0xB0);
        pmt.put_u8(0x17); // section_length: 19 data + 4 CRC32
        pmt.put_u16(0x0001);
        pmt.put_u8(0xC1);
        pmt.put_u8(0x00);
        pmt.put_u8(0x00);
        pmt.put_u16(0xE000 | VIDEO_PID); // PCR_PID=video
        pmt.put_u16(0xF000);
        pmt.put_u8(video_stream_type); // H.264=0x1B / H.265=0x24
        pmt.put_u16(0xE000 | VIDEO_PID);
        pmt.put_u16(0xF000);
        pmt.put_u8(0x0F); // AAC
        pmt.put_u16(0xE000 | AUDIO_PID);
        pmt.put_u16(0xF000);
        let crc = crc32(pmt.as_ref());
        pmt.put_u32(crc);
        let mut pmt_payload = BytesMut::with_capacity(32);
        pmt_payload.put_u8(0x00); // pointer_field
        pmt_payload.extend_from_slice(&pmt);
        write_ts_packet(out, PMT_PID, &pmt_payload, true, &mut self.continuity_pmt);
    }

    pub(crate) fn write_pes(
        &mut self,
        out: &mut Vec<u8>,
        pid: u16,
        pes: &[u8],
        rand_access: bool,
        dts_ms: u64,
    ) {
        self.pcr = dts_ms * 27000;
        let pcr = self.pcr;
        write_pes_to_ts(out, pid, pes, rand_access, self.continuity_for(pid), pcr);
    }

    fn continuity_for(&mut self, pid: u16) -> &mut u8 {
        match pid {
            VIDEO_PID => &mut self.continuity_video,
            AUDIO_PID => &mut self.continuity_audio,
            _ => unreachable!(),
        }
    }
}

pub struct HlsMuxer {
    target_duration: f64,
    max_segments: usize,
    current_frames: Vec<MediaFrame>,
    segment_start_pts: Option<u64>,
    segment_index: u32,
    writer: TsWriter,
    video_config_sent: bool,
    audio_config_sent: bool,
    video_config_data: Option<Vec<u8>>,
    audio_config_data: Option<Vec<u8>>,
    video_codec: Option<CodecId>,
}

impl HlsMuxer {
    pub fn new(target_duration: f64) -> Self {
        Self {
            target_duration,
            max_segments: 6,
            current_frames: Vec::new(),
            segment_start_pts: None,
            segment_index: 0,
            writer: TsWriter::new(),
            video_config_sent: false,
            audio_config_sent: false,
            video_config_data: None,
            audio_config_data: None,
            video_codec: None,
        }
    }

    pub fn with_max_segments(mut self, n: usize) -> Self {
        self.max_segments = n;
        self
    }

    pub fn push_frame(&mut self, frame: MediaFrame) -> Option<(u32, Vec<u8> /*data*/)> {
        if self.segment_start_pts.is_none() {
            self.segment_start_pts = Some(frame.dts);
        }

        if frame.frame_type == FrameType::Video && frame.key_frame {
            if let Some(start_pts) = self.segment_start_pts {
                let elapsed = (frame.dts.saturating_sub(start_pts)) as f64 / 1000.0;
                debug!(
                    "HLS keyframe check: elapsed={:.2} target={:.2} frames={}",
                    elapsed,
                    self.target_duration,
                    self.current_frames.len()
                );
                if elapsed >= self.target_duration && !self.current_frames.is_empty() {
                    let data = self.mux_segment();
                    let idx = self.segment_index;
                    self.segment_index += 1;
                    self.current_frames.clear();
                    self.video_config_sent = false;
                    self.audio_config_sent = false;
                    self.segment_start_pts = Some(frame.dts);
                    self.current_frames.push(frame);
                    return Some((idx, data));
                }
            }
        }

        self.current_frames.push(frame);
        None
    }

    pub fn flush(&mut self) -> Option<(u32, Vec<u8>)> {
        if self.current_frames.is_empty() {
            return None;
        }
        let data = self.mux_segment();
        let idx = self.segment_index;
        self.segment_index += 1;
        self.current_frames.clear();
        self.video_config_sent = false;
        self.audio_config_sent = false;
        self.segment_start_pts = None;
        Some((idx, data))
    }

    fn mux_segment(&mut self) -> Vec<u8> {
        let mut out = Vec::new();

        info!("mux_segment: {} frames", self.current_frames.len());
        for (i, frame) in self.current_frames.iter().enumerate() {
            match frame.frame_type {
                FrameType::Video => {
                    let is_cfg = is_video_config(frame);
                    let is_kf = frame.key_frame;
                    info!(
                        "  frame[{}] Video: key={} config={} data_len={} first_bytes={:02x?}",
                        i,
                        is_kf,
                        is_cfg,
                        frame.data.len(),
                        &frame.data[..frame.data.len().min(5)]
                    );
                    if is_cfg {
                        self.video_codec = Some(frame.codec);
                        let cfg = extract_video_config(frame);
                        info!(
                            "  -> extracted config: {} bytes, {:02x?}",
                            cfg.len(),
                            &cfg[..cfg.len().min(20)]
                        );
                        self.video_config_data = Some(cfg);
                        self.video_config_sent = false;
                    }
                }
                FrameType::Audio if is_aac_config(frame) => {
                    self.audio_config_data = Some(extract_aac_config(frame));
                    self.audio_config_sent = false;
                }
                _ => {}
            }
        }

        let video_stream_type = match self.video_codec {
            Some(CodecId::H265) => 0x24,
            _ => 0x1B,
        };
        self.writer.write_pat_pmt(&mut out, video_stream_type);

        if !self.video_config_sent {
            if let Some(ref config) = self.video_config_data {
                info!("Writing video config PES: {} bytes", config.len());
                let pes = build_config_pes(0xE0, config);
                self.writer
                    .write_pes(&mut out, VIDEO_PID, &pes, true, self.writer.pcr / 90);
                self.video_config_sent = true;
            } else {
                info!("No video config data available");
            }
        }

        if !self.audio_config_sent {
            if let Some(ref config) = self.audio_config_data {
                let pes = build_config_pes(0xC0, config);
                self.writer
                    .write_pes(&mut out, AUDIO_PID, &pes, true, self.writer.pcr / 90);
                self.audio_config_sent = true;
            }
        }

        for frame in &self.current_frames {
            match frame.frame_type {
                FrameType::Video => {
                    if !is_video_config(frame) {
                        let annex_b = video_sample_annex_b(frame)
                            .map(|data| data.to_vec())
                            .unwrap_or_default();
                        if !annex_b.is_empty() {
                            info!(
                                "  -> annex_b: {} bytes, first 12: {:02x?}",
                                annex_b.len(),
                                &annex_b[..annex_b.len().min(12)]
                            );
                            let pes = build_data_pes_raw(0xE0, &annex_b, frame.timestamp);
                            self.writer.write_pes(
                                &mut out,
                                VIDEO_PID,
                                &pes,
                                true,
                                frame.timestamp as u64,
                            );
                        } else {
                            info!(
                                "  -> annex_b EMPTY for frame data[:5]={:02x?}",
                                &frame.data[..frame.data.len().min(5)]
                            );
                        }
                    }
                }
                FrameType::Audio if !is_aac_config(frame) => {
                    let audio_data = aac_frame_to_adts(frame, self.audio_config_data.as_deref());
                    if !audio_data.is_empty() {
                        let pes = build_data_pes_raw(0xC0, &audio_data, frame.timestamp);
                        self.writer.write_pes(
                            &mut out,
                            AUDIO_PID,
                            &pes,
                            false,
                            frame.timestamp as u64,
                        );
                    }
                }
                _ => {}
            }
        }

        out
    }
}

pub(crate) fn build_config_pes(stream_id: u8, config: &[u8]) -> Vec<u8> {
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    let total_pes_len = config.len() + 3;
    pes.push(((total_pes_len >> 8) & 0xFF) as u8);
    pes.push((total_pes_len & 0xFF) as u8);
    pes.push(0x84); // '10', scrambling=00, priority=0, alignment=1
    pes.push(0x00); // PTS/DTS flags=00
    pes.push(0x00); // PES_header_data_length=0
    pes.extend_from_slice(config);
    pes
}

pub(crate) fn build_data_pes_raw(stream_id: u8, data: &[u8], timestamp: u32) -> Vec<u8> {
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    let pts_dts_len: u8 = 5;
    let total_pes_len = data.len() + pts_dts_len as usize + 3;
    pes.push(((total_pes_len >> 8) & 0xFF) as u8);
    pes.push((total_pes_len & 0xFF) as u8);
    pes.push(0x84);
    pes.push(0x80);
    pes.push(pts_dts_len);
    let pts = timestamp as u64 * 90;
    write_pts(&mut pes, 0x21, pts);
    pes.extend_from_slice(data);
    pes
}

fn make_adts_header(frame_len: usize, audio_config: &[u8]) -> [u8; 7] {
    if audio_config.len() < 2 {
        return [0u8; 7];
    }
    let combined = ((audio_config[0] as u16) << 8) | (audio_config[1] as u16);
    let audio_object_type = ((combined >> 11) & 0x1F) as u8;
    let freq_index = ((combined >> 7) & 0x0F) as u8;
    let channels = ((combined >> 3) & 0x0F) as u8;
    let profile = audio_object_type.saturating_sub(1);
    let total_len = frame_len + 7;
    [
        0xFF,
        0xF1,
        (profile << 6) | (freq_index << 2) | ((channels >> 2) & 0x01),
        ((channels & 0x03) << 6) | (((total_len >> 11) & 0x03) as u8),
        ((total_len >> 3) & 0xFF) as u8,
        (((total_len & 0x07) << 5) | 0x1F) as u8,
        0x00,
    ]
}

/// Normalize an AAC media frame to the ADTS form required by MPEG-TS.
///
/// SRT/GB28181 sources may already provide ADTS, while RTMP/RTSP sources carry
/// raw AAC behind an FLV header. Keeping this normalization here makes both
/// inputs produce the same PES payload without double-wrapping ADTS frames.
pub(crate) fn aac_frame_to_adts(frame: &MediaFrame, audio_config: Option<&[u8]>) -> Vec<u8> {
    if frame.codec != CodecId::AAC || frame.config_frame {
        return Vec::new();
    }
    if frame.payload_format == PayloadFormat::Adts {
        return frame.data.to_vec();
    }
    let Ok(raw) = aac_raw_payload(frame) else {
        return Vec::new();
    };
    let Some(config) = audio_config.filter(|config| config.len() >= 2) else {
        return Vec::new();
    };
    let adts = make_adts_header(raw.len(), config);
    let mut out = Vec::with_capacity(adts.len() + raw.len());
    out.extend_from_slice(&adts);
    out.extend_from_slice(&raw);
    out
}

fn write_pcr(buf: &mut Vec<u8>, pcr: u64) {
    let base = pcr / 300;
    let ext = (pcr % 300) as u16;
    buf.push((base >> 25) as u8);
    buf.push((base >> 17) as u8);
    buf.push((base >> 9) as u8);
    buf.push((base >> 1) as u8);
    buf.push(((base & 1) as u8) << 7 | 0x7E);
    buf.push(((ext >> 8) as u8 & 0x01) | (ext & 0xFF) as u8);
}

fn write_pes_to_ts(
    out: &mut Vec<u8>,
    pid: u16,
    pes: &[u8],
    rand_access: bool,
    continuity: &mut u8,
    pcr: u64,
) {
    let mut offset = 0;
    let max_payload = TS_PACKET_SIZE - 4; // 184

    // First packet
    let first_chunk = pes.len().min(max_payload);
    let first_pad = max_payload - first_chunk;

    // Build adaptation field for first packet
    let mut adapt: Vec<u8> = Vec::new();
    if first_pad > 0 || rand_access {
        if rand_access && first_pad >= 8 {
            // Random access: write adaptation with random_access flag + PCR
            let adapt_data_len = first_pad - 1;
            adapt.push(adapt_data_len as u8);
            adapt.push(0x50); // random_access=1, PCR flag=1
            write_pcr(&mut adapt, pcr);
            adapt.extend(std::iter::repeat_n(0xFF, adapt_data_len.saturating_sub(7)));
        } else if first_pad > 0 {
            // Just padding, no PCR, no random access
            let adapt_data_len = first_pad - 1; // -1 for length byte itself
            adapt.push(adapt_data_len as u8); // adaptation_field_length
            if adapt_data_len > 0 {
                adapt.push(0x00); // no flags
                adapt.extend(std::iter::repeat_n(0xFF, adapt_data_len.saturating_sub(1)));
            }
        } else {
            // rand_access but no room for padding — minimal adaptation with just rand_access flag
            adapt.push(0x01); // adaptation_field_length=1
            adapt.push(0x40); // random_access=1
        }
    }

    let adapt_total = adapt.len(); // includes the length byte
    let payload_len = max_payload - adapt_total;
    write_ts_packet_with(
        out,
        pid,
        if adapt_total > 0 { Some(&adapt) } else { None },
        &pes[..payload_len],
        true,
        continuity,
    );
    offset += payload_len;

    // Continuation packets
    while offset < pes.len() {
        let remaining = pes.len() - offset;
        let chunk = remaining.min(max_payload);
        let pad = max_payload - chunk;

        let adapt = if pad > 0 {
            let adapt_data_len = pad - 1; // -1 for length byte
            let mut a = Vec::with_capacity(pad);
            a.push(adapt_data_len as u8); // adaptation_field_length
            if adapt_data_len > 0 {
                a.push(0x00); // no flags
                a.extend(std::iter::repeat_n(0xFF, adapt_data_len.saturating_sub(1)));
            }
            Some(a)
        } else {
            None
        };

        let adapt_len = adapt.as_ref().map(|a| a.len()).unwrap_or(0);
        let actual_payload = max_payload - adapt_len;
        write_ts_packet_with(
            out,
            pid,
            adapt.as_deref(),
            &pes[offset..offset + actual_payload],
            false,
            continuity,
        );
        offset += actual_payload;
    }
}

fn write_ts_packet(out: &mut Vec<u8>, pid: u16, payload: &[u8], start: bool, continuity: &mut u8) {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = 0x47;
    pkt[1] = ((pid >> 8) & 0x1F) as u8 | if start { 0x40 } else { 0x00 };
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10 | (*continuity & 0x0F);
    *continuity = (*continuity + 1) & 0x0F;
    let end = (4 + payload.len()).min(TS_PACKET_SIZE);
    pkt[4..end].copy_from_slice(&payload[..(end - 4)]);
    out.extend_from_slice(&pkt);
}

fn write_ts_packet_with(
    out: &mut Vec<u8>,
    pid: u16,
    adaptation: Option<&[u8]>,
    payload: &[u8],
    start: bool,
    continuity: &mut u8,
) {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = 0x47;
    pkt[1] = ((pid >> 8) & 0x1F) as u8 | if start { 0x40 } else { 0x00 };
    pkt[2] = (pid & 0xFF) as u8;

    let adapt_len = adaptation.map(|a| a.len()).unwrap_or(0);
    let payload_len = payload.len();

    if adapt_len > 0 && payload_len > 0 {
        pkt[3] = 0x30 | (*continuity & 0x0F);
    } else if adapt_len > 0 {
        pkt[3] = 0x20 | (*continuity & 0x0F);
    } else {
        pkt[3] = 0x10 | (*continuity & 0x0F);
    }

    *continuity = (*continuity + 1) & 0x0F;

    let mut pos = 4;
    if let Some(adapt) = adaptation {
        for &b in adapt {
            if pos < TS_PACKET_SIZE {
                pkt[pos] = b;
                pos += 1;
            }
        }
    }
    for &b in payload {
        if pos < TS_PACKET_SIZE {
            pkt[pos] = b;
            pos += 1;
        }
    }

    out.extend_from_slice(&pkt);
}

fn write_pts(buf: &mut Vec<u8>, marker: u8, pts: u64) {
    buf.push(marker | ((pts >> 29) as u8 & 0x0E) | 0x01);
    buf.push(((pts >> 22) & 0xFF) as u8);
    buf.push(((pts >> 14) & 0xFE) as u8 | 0x01);
    buf.push(((pts >> 7) & 0xFF) as u8);
    buf.push(((pts << 1) & 0xFE) as u8 | 0x01);
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if crc & 0x80000000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub(crate) fn is_video_config(frame: &MediaFrame) -> bool {
    frame.config_frame
        || (matches!(
            frame.payload_format,
            PayloadFormat::Flv | PayloadFormat::Unknown
        ) && frame.data.len() >= 2
            && (frame.data[0] & 0x0F == 7 || frame.data[0] & 0x0F == 12)
            && frame.data[1] & 0x0F == 0)
}

pub(crate) fn extract_video_config(frame: &MediaFrame) -> Vec<u8> {
    let Ok(config) = video_decoder_config(frame) else {
        return Vec::new();
    };
    if frame.codec == CodecId::H265 {
        extract_hevc_config_record(&config)
    } else {
        extract_h264_config_record(&config)
    }
}

pub(crate) fn is_aac_config(frame: &MediaFrame) -> bool {
    frame.config_frame
        || (matches!(
            frame.payload_format,
            PayloadFormat::Flv | PayloadFormat::Unknown
        ) && frame.data.len() >= 2
            && (frame.data[0] >> 4) == 10
            && frame.data[1] & 0x0F == 0)
}

fn extract_h264_config_record(data: &[u8]) -> Vec<u8> {
    let mut config = Vec::new();
    if data.len() < 6 {
        return config;
    }

    let num_sps = (data[5] & 0x1F) as usize;
    let mut pos = 6;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            break;
        }
        let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + nalu_len > data.len() {
            break;
        }
        config.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        config.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }
    if pos < data.len() {
        let num_pps = data[pos] as usize;
        pos += 1;
        for _ in 0..num_pps {
            if pos + 2 > data.len() {
                break;
            }
            let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + nalu_len > data.len() {
                break;
            }
            config.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            config.extend_from_slice(&data[pos..pos + nalu_len]);
            pos += nalu_len;
        }
    }
    config
}

fn extract_hevc_config_record(data: &[u8]) -> Vec<u8> {
    // `data` is a HEVCDecoderConfigurationRecord (hvcC) without an FLV header.
    let mut config = Vec::new();
    if data.len() < 23 {
        return config;
    }
    let mut pos = 22;
    let num_arrays = data[pos] as usize;
    pos += 1;
    for _ in 0..num_arrays {
        if pos + 3 > data.len() {
            break;
        }
        let nal_type = data[pos] & 0x3F;
        let num_nalus = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
        pos += 3;
        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                break;
            }
            let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + nalu_len > data.len() {
                break;
            }
            // Only VPS (32), SPS (33) and PPS (34) go into the in-band config.
            if nal_type == 32 || nal_type == 33 || nal_type == 34 {
                config.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                config.extend_from_slice(&data[pos..pos + nalu_len]);
            }
            pos += nalu_len;
        }
    }
    config
}

pub(crate) fn extract_aac_config(frame: &MediaFrame) -> Vec<u8> {
    aac_audio_specific_config(frame)
        .map(|config| config.to_vec())
        .unwrap_or_default()
}

impl Default for HlsMuxer {
    fn default() -> Self {
        Self::new(2.0)
    }
}

pub fn start_hls_task(
    source: Arc<MediaSource>,
    segments: Arc<RwLock<VecDeque<HlsSegment>>>,
    target_duration: f64,
    max_segments: usize,
    on_stop: Option<Box<dyn FnOnce() + Send>>,
) {
    tokio::spawn(async move {
        let mut muxer = HlsMuxer::new(target_duration).with_max_segments(max_segments);

        let config_frames = source.get_cached_config_frames().await;
        for frame in &config_frames {
            debug!(
                "HLS replay config frame: type={:?} key={} config={} ts={}",
                frame.frame_type, frame.key_frame, frame.config_frame, frame.timestamp
            );
            let _ = muxer.push_frame(frame.clone());
        }
        info!(
            "HLS replayed {} config frames for {}",
            config_frames.len(),
            source.url()
        );

        let mut rx = source.subscribe();

        info!("HLS task started for {}, entering recv loop", source.url());

        loop {
            match rx.recv().await {
                Ok(frame) => {
                    info!(
                        "HLS recv frame: type={:?} key={} config={} ts={}",
                        frame.frame_type, frame.key_frame, frame.config_frame, frame.timestamp
                    );
                    if let Some((idx, data)) = muxer.push_frame(frame) {
                        let seg = HlsSegment {
                            index: idx,
                            duration: target_duration,
                            data,
                        };
                        let mut segs = segments.write().await;
                        segs.push_back(seg);
                        while segs.len() > max_segments {
                            segs.pop_front();
                        }
                        info!("HLS segment {} generated ({} total)", idx, segs.len());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("HLS task lagged by {} frames, continuing", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("HLS task broadcast channel closed for {}", source.url());
                    break;
                }
            }
        }

        if let Some((idx, data)) = muxer.flush() {
            let seg = HlsSegment {
                index: idx,
                duration: target_duration,
                data,
            };
            let mut segs = segments.write().await;
            segs.push_back(seg);
        }

        if let Some(cb) = on_stop {
            cb();
        }

        info!("HLS task ended for {}", source.url());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Builds a minimal FLV HEVC video tag body carrying an
    /// HEVCDecoderConfigurationRecord with one VPS (32), SPS (33) and PPS (34).
    fn make_hevc_config_data() -> Vec<u8> {
        let mut d = vec![
            0x1C, 0x00, 0x00, 0x00, 0x00, // frame type/codec(12) + packet type 0 + ctime
            0x01, // configurationVersion
            0x20, // profile_space/tier/profile_idc
            0x00, 0x00, 0x00, 0x00, // general_profile_compatibility_flags
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // general_constraint_indicator_flags
            0x5D, // general_level_idc
            0x00, 0x00, // min_spatial_segmentation_idc
            0x00, // parallelismType
            0x01, // chromaFormat
            0x00, // bitDepthLumaMinus8
            0x00, // bitDepthChromaMinus8
            0x00, 0x00, // avgFrameRate (2 bytes)
            0x03, // lengthSizeMinusOne = 1, temporalIdNested = 1
            0x03, // numOfArrays = 3
        ];
        // array 1: VPS
        d.extend_from_slice(&[32, 0, 1, 0, 2, 0xAA, 0xBB]);
        // array 2: SPS
        d.extend_from_slice(&[33, 0, 1, 0, 2, 0xCC, 0xDD]);
        // array 3: PPS
        d.extend_from_slice(&[34, 0, 1, 0, 2, 0xEE, 0xFF]);
        d
    }

    #[test]
    fn hevc_config_extraction() {
        let d = make_hevc_config_data();
        let cfg = extract_hevc_config_record(&d[5..]);
        assert_eq!(
            cfg,
            vec![0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC, 0xDD, 0, 0, 0, 1, 0xEE, 0xFF]
        );
    }

    #[test]
    fn hevc_segment_pmt_stream_type() {
        let mut muxer = HlsMuxer::new(2.0);

        let cfg_frame = MediaFrame::new_video(
            0,
            CodecId::H265,
            0,
            0,
            0,
            Bytes::from(make_hevc_config_data()),
            false,
        );
        // A HEVC IDR access unit (packet type 1, single 4-byte NALU).
        let idr_data = vec![
            0x1C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x40, 0x01, 0x02, 0x03,
        ];
        let idr_frame =
            MediaFrame::new_video(0, CodecId::H265, 0, 0, 0, Bytes::from(idr_data), true);

        muxer.push_frame(cfg_frame);
        muxer.push_frame(idr_frame);

        let (_, seg) = muxer.flush().unwrap();
        // PMT advertises HEVC (0x24) for the video PID (0xE100) with no
        // program-info descriptors (0xF000) preceding the stream type.
        let pattern = [0xE1, 0x00, 0xF0, 0x00, 0x24];
        assert!(
            seg.windows(5).any(|w| w == pattern),
            "PMT should advertise HEVC stream type 0x24"
        );
    }
}
