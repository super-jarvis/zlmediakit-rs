//! MP4 demuxer (ISO base media file format).
//!
//! Parses an MP4 file produced by [`crate::Mp4Muxer`] (or any standards
//! compliant ftyp/moov/mdat file) and reconstructs the original encoded
//! samples as [`DemuxedSample`]s. Samples can then be converted into
//! [`MediaFrame`]s in the internal FLV-style framing used by the rest of the
//! server, so a recorded MP4 can be replayed over any protocol (VOD).
//!
//! Supported codecs: H.264 (avc1), H.265 (hvc1/hev1), AAC (mp4a).
//!
//! Boxes parsed: `ftyp`, `moov` → `mvhd`, `trak` → `tkhd`, `mdia` → `mdhd`,
//! `hdlr`, `minf` → `stbl` → `stsd`, `stts`, `stsc`, `stsz`, `stco`/`co64`,
//! `stss`. Sample data is read from `mdat`.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::path::Path;
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame, PayloadFormat};

/// A single decoded sample (one encoded frame / access unit) from the file.
#[derive(Debug, Clone)]
pub struct DemuxedSample {
    pub track_id: u32,
    pub codec: CodecId,
    /// Raw sample payload as stored in the file (length-prefixed NALs for
    /// video in AVCC format, raw AAC data for audio).
    pub data: Bytes,
    /// Presentation timestamp in milliseconds (media-bus clock).
    pub pts_ms: u32,
    /// Decode timestamp in milliseconds.
    pub dts_ms: u32,
    /// Duration in milliseconds.
    pub duration_ms: u32,
    /// Whether this is a random-access / key frame.
    pub is_sync: bool,
    /// True for the decoder configuration record sample (SPS/PPS / ASC).
    pub is_config: bool,
}

/// One media track parsed from the file.
#[derive(Debug, Clone)]
pub struct DemuxedTrack {
    pub track_id: u32,
    pub handler: TrackKind,
    pub codec: CodecId,
    /// Track timescale (ticks per second for this track's timestamps).
    pub timescale: u32,
    /// Track duration in track timescale ticks.
    pub duration: u64,
    /// Decoder configuration record payload (avcC / hvcC / esds-derived ASC).
    pub config: Bytes,
    /// Total number of samples in this track.
    pub sample_count: u32,
    /// Pixel width (video) or 0 if unknown.
    pub width: u32,
    /// Pixel height (video) or 0 if unknown.
    pub height: u32,
    /// Nominal frame rate in fps (video) or 0 if unknown.
    pub fps: f64,
    /// Audio sample rate in Hz (audio) or 0 if unknown.
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

/// A parsed MP4 file.
pub struct Mp4Demuxer {
    tracks: BTreeMap<u32, DemuxedTrack>,
    /// samples grouped by track id, in decode order.
    samples: BTreeMap<u32, Vec<DemuxedSample>>,
}

impl Mp4Demuxer {
    /// Opens and fully parses an MP4 file from disk.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let data = tokio::fs::read(path.as_ref())
            .await
            .with_context(|| format!("failed to read mp4 file {}", path.as_ref().display()))?;
        Self::from_bytes(&data)
    }

    /// Parses an MP4 from an in-memory buffer.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let parser = BoxCursor::new(data);
        let top = parser.walk_top_boxes()?;

        let moov = top.get("moov").context("missing moov box")?;
        top.get("mdat").context("missing mdat box")?;

        let moov_boxes = BoxCursor::new(moov).walk_top_boxes_vec()?;
        let mvhd = moov_boxes
            .iter()
            .find(|(n, _)| n == "mvhd")
            .map(|(_, b)| *b)
            .context("missing mvhd")?;
        let _movie_timescale = read_mvhd_timescale(mvhd);

        let mut tracks = BTreeMap::new();
        let mut samples: BTreeMap<u32, Vec<DemuxedSample>> = BTreeMap::new();

        for (name, body) in moov_boxes.iter() {
            if name != "trak" {
                continue;
            }
            let trak = parse_trak(body, data);
            if let Ok(Some((track, track_samples))) = trak {
                samples.insert(track.track_id, track_samples);
                tracks.insert(track.track_id, track);
            }
        }

        if tracks.is_empty() {
            bail!("no supported media tracks found in mp4");
        }

        Ok(Self { tracks, samples })
    }

    /// Returns metadata for all tracks.
    pub fn tracks(&self) -> Vec<DemuxedTrack> {
        self.tracks.values().cloned().collect()
    }

    /// Returns all samples for a given track id, in decode order.
    pub fn samples_for(&self, track_id: u32) -> Option<&[DemuxedSample]> {
        self.samples.get(&track_id).map(|v| v.as_slice())
    }

    /// Returns all samples across all tracks, in decode order, grouped by
    /// track. Useful for sequential replay interleaving.
    pub fn all_samples(&self) -> &BTreeMap<u32, Vec<DemuxedSample>> {
        &self.samples
    }

    /// Converts every sample into a [`MediaFrame`] in the internal FLV-style
    /// framing (config frame first, then data frames) suitable for publishing
    /// to a [`zlmediakit_core::MediaSource`].
    pub fn to_media_frames(&self) -> Vec<MediaFrame> {
        let mut frames = Vec::new();
        for track in self.tracks.values() {
            let samples = match self.samples.get(&track.track_id) {
                Some(s) => s,
                None => continue,
            };

            if let Some(cfg) = self.make_config_frame(track) {
                frames.push(cfg);
            }

            for s in samples {
                if let Some(f) = self.make_data_frame(track, s) {
                    frames.push(f);
                }
            }
        }
        frames
    }

    /// Builds the config (SPS/PPS/ASC) [`MediaFrame`] for a track, if present.
    fn make_config_frame(&self, track: &DemuxedTrack) -> Option<MediaFrame> {
        if track.config.is_empty() {
            return None;
        }
        let cfg_data = build_flv_config(track, &track.config);
        let mut frame = match track.handler {
            TrackKind::Video => MediaFrame::new_video(
                track.track_id,
                track.codec,
                0,
                0,
                0,
                Bytes::from(cfg_data),
                true,
            ),
            TrackKind::Audio => {
                MediaFrame::new_audio(track.track_id, track.codec, 0, 0, 0, Bytes::from(cfg_data))
            }
            TrackKind::Other => return None,
        }
        .with_payload_format(PayloadFormat::Flv);
        frame.config_frame = true;
        Some(frame)
    }

    /// Builds a data [`MediaFrame`] for a single sample of a track.
    fn make_data_frame(&self, track: &DemuxedTrack, s: &DemuxedSample) -> Option<MediaFrame> {
        let data = build_flv_sample(track, &s.data, s.is_sync);
        let frame = match track.handler {
            TrackKind::Video => MediaFrame::new_video(
                track.track_id,
                track.codec,
                s.pts_ms,
                s.pts_ms as u64,
                s.dts_ms as u64,
                Bytes::from(data),
                s.is_sync,
            ),
            TrackKind::Audio => MediaFrame::new_audio(
                track.track_id,
                track.codec,
                s.pts_ms,
                s.pts_ms as u64,
                s.dts_ms as u64,
                Bytes::from(data),
            ),
            TrackKind::Other => return None,
        }
        .with_payload_format(PayloadFormat::Flv);
        Some(frame)
    }

    /// Returns all samples across all tracks, sorted by their presentation
    /// timestamp so audio and video are interleaved in playback order. Config
    /// frames are emitted first (one per track), matching the FLV convention
    /// expected by players.
    pub fn interleaved_media_frames(&self) -> Vec<MediaFrame> {
        let mut frames = Vec::new();
        // Config frames first, in track-id order.
        for track in self.tracks.values() {
            if let Some(cfg) = self.make_config_frame(track) {
                frames.push(cfg);
            }
        }
        // Then interleave data samples by presentation timestamp.
        let mut samples: Vec<(u32, &DemuxedSample)> = Vec::new();
        for (track_id, track_samples) in self.samples.iter() {
            for s in track_samples.iter() {
                samples.push((*track_id, s));
            }
        }
        samples.sort_by_key(|(_, s)| s.pts_ms);
        for (track_id, s) in samples {
            if let Some(track) = self.tracks.get(&track_id) {
                if let Some(f) = self.make_data_frame(track, s) {
                    frames.push(f);
                }
            }
        }
        frames
    }
}

// ── box primitives ──────────────────────────────────────────────────────

struct BoxCursor<'a> {
    data: &'a [u8],
}

impl<'a> BoxCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// Returns top-level boxes keyed by 4-char type, with slices into the
    /// underlying buffer.
    fn walk_top_boxes(&self) -> Result<BTreeMap<String, &'a [u8]>> {
        let mut map = BTreeMap::new();
        for (name, body) in self.walk_top_boxes_vec()? {
            map.insert(name, body);
        }
        Ok(map)
    }

    /// Same as [`walk_top_boxes`] but preserves all boxes in document order,
    /// including siblings that share the same 4-char type (e.g. multiple `trak` boxes).
    fn walk_top_boxes_vec(&self) -> Result<Vec<(String, &'a [u8])>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 8 <= self.data.len() {
            let (size, hdr) = read_box_header(&self.data[pos..])?;
            if size < 8 {
                break;
            }
            let body_size = size as usize - hdr;
            if pos + size as usize > self.data.len() {
                break;
            }
            let box_type = &self.data[pos + 4..pos + 8];
            let name = String::from_utf8_lossy(box_type).to_string();
            let body_start = pos + hdr;
            let body_end = body_start + body_size;
            out.push((name, &self.data[body_start..body_end]));
            pos += size as usize;
        }
        Ok(out)
    }
}

/// Reads a box header. Returns (total_size, header_len). header_len is 8 for
/// normal boxes, 16 for 64-bit size boxes (size field == 1).
fn read_box_header(data: &[u8]) -> Result<(u64, usize)> {
    if data.len() < 8 {
        bail!("truncated box header");
    }
    let size32 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64;
    if size32 == 1 {
        if data.len() < 16 {
            bail!("truncated 64-bit box header");
        }
        let size64 = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        Ok((size64, 16))
    } else {
        Ok((size32, 8))
    }
}

fn read_mvhd_timescale(mvhd: &[u8]) -> u32 {
    // body = version/flags(4) + creation(4) + modification(4) + timescale(4).
    if mvhd.len() >= 16 {
        u32::from_be_bytes([mvhd[12], mvhd[13], mvhd[14], mvhd[15]])
    } else {
        1000
    }
}

fn parse_trak(trak: &[u8], file_data: &[u8]) -> Result<Option<(DemuxedTrack, Vec<DemuxedSample>)>> {
    let boxes = BoxCursor::new(trak).walk_top_boxes()?;

    let tkhd = boxes.get("tkhd").context("missing tkhd")?;
    let track_id = read_tkhd_track_id(tkhd);

    let mdia = match boxes.get("mdia") {
        Some(m) => m,
        None => return Ok(None),
    };
    let mdia_boxes = BoxCursor::new(mdia).walk_top_boxes()?;

    let mdhd = mdia_boxes.get("mdhd").context("missing mdhd")?;
    let timescale = read_mdhd_timescale(mdhd);
    if timescale == 0 {
        bail!("mp4 track timescale must be positive");
    }
    let duration = read_mdhd_duration(mdhd);

    let hdlr = mdia_boxes.get("hdlr").context("missing hdlr")?;
    let handler = read_hdlr_type(hdlr);
    if handler == TrackKind::Other {
        return Ok(None);
    }

    let minf = match mdia_boxes.get("minf") {
        Some(m) => m,
        None => return Ok(None),
    };
    let minf_boxes = BoxCursor::new(minf).walk_top_boxes()?;
    // Some muxers nest sample tables under a `stbl` box; others (including
    // this crate's Mp4Muxer) place them directly under `minf`. Support both.
    let stbl_boxes = if let Some(stbl) = minf_boxes.get("stbl") {
        BoxCursor::new(stbl).walk_top_boxes()?
    } else {
        minf_boxes.clone()
    };

    let stsd = stbl_boxes.get("stsd").context("missing stsd")?;
    let (codec, config) = parse_stsd(stsd, handler)?;

    let stsz = stbl_boxes.get("stsz").context("missing stsz")?;
    let sample_sizes = parse_stsz(stsz, file_data.len())?;
    let sample_count = sample_sizes.len();
    if sample_count == 0 {
        return Ok(None);
    }

    let stts = stbl_boxes.get("stts").context("missing stts")?;
    let durations = parse_stts(stts, sample_count)?;

    let stsc = stbl_boxes.get("stsc").context("missing stsc")?;
    let chunk_sample_entries = parse_stsc(stsc)?;

    let (chunk_offsets, co64) = if let Some(c) = stbl_boxes.get("co64") {
        (parse_co64(c)?, true)
    } else {
        let stco = stbl_boxes.get("stco").context("missing stco")?;
        (parse_stco(stco)?, false)
    };
    let _ = co64;

    let sync_samples = if let Some(stss) = stbl_boxes.get("stss") {
        parse_stss(stss)?
    } else {
        // No stss → every sample is a sync sample.
        Vec::new()
    };

    // Build per-sample absolute timestamps (in track timescale ticks).
    let mut timestamps = Vec::with_capacity(sample_count);
    let mut t = 0u64;
    for d in &durations {
        timestamps.push(t);
        t = t.saturating_add(*d as u64);
    }

    // Map chunk -> sample range, then compute absolute file offsets.
    let mut samples_out = Vec::with_capacity(sample_count);
    let mut sample_idx = 0usize;
    for (chunk_idx, chunk_offset) in chunk_offsets.iter().copied().enumerate() {
        let chunk_number = chunk_idx as u32 + 1;
        let Some((_, samples_in_chunk, _)) = chunk_sample_entries
            .iter()
            .rev()
            .find(|(first_chunk, _, _)| *first_chunk <= chunk_number)
        else {
            continue;
        };
        let chunk_offset =
            usize::try_from(chunk_offset).context("mp4 chunk offset is too large")?;
        let mut running = 0usize;
        for _ in 0..*samples_in_chunk {
            if sample_idx >= sample_count {
                break;
            }
            let size = sample_sizes[sample_idx] as usize;
            let Some(offset) = chunk_offset.checked_add(running) else {
                bail!("mp4 sample offset overflow");
            };
            let Some(end) = offset.checked_add(size) else {
                bail!("mp4 sample size overflow");
            };
            running = running
                .checked_add(size)
                .context("mp4 chunk sample sizes overflow")?;
            let data = if end <= file_data.len() {
                Bytes::copy_from_slice(&file_data[offset..end])
            } else {
                Bytes::new()
            };
            let tick = timestamps[sample_idx];
            let pts_ms = (tick * 1000 / timescale as u64) as u32;
            let dur_ms = ((*durations.get(sample_idx).unwrap_or(&0) as u64) * 1000
                / timescale as u64) as u32;
            let is_sync = if sync_samples.is_empty() {
                true
            } else {
                sync_samples.binary_search(&(sample_idx as u32 + 1)).is_ok()
            };
            samples_out.push(DemuxedSample {
                track_id,
                codec,
                data,
                pts_ms,
                dts_ms: pts_ms,
                duration_ms: dur_ms,
                is_sync,
                is_config: false,
            });
            sample_idx += 1;
        }
    }

    let (width, height, fps, sample_rate) = derive_track_params(handler, codec, &config);

    let track = DemuxedTrack {
        track_id,
        handler,
        codec,
        timescale,
        duration,
        config: Bytes::from(config),
        sample_count: sample_count as u32,
        width,
        height,
        fps,
        sample_rate,
    };

    Ok(Some((track, samples_out)))
}

/// Derives display parameters (video size / fps, audio sample rate) from the
/// decoder configuration record.
fn derive_track_params(handler: TrackKind, codec: CodecId, config: &[u8]) -> (u32, u32, f64, u32) {
    match handler {
        TrackKind::Video => {
            if codec == CodecId::H264 || codec == CodecId::H265 {
                if let Some((w, h)) = extract_video_dimensions(codec, config) {
                    return (w, h, 30.0, 0);
                }
            }
            (0, 0, 0.0, 0)
        }
        TrackKind::Audio => {
            if codec == CodecId::AAC {
                if let Some(sr) = extract_audio_sample_rate(config) {
                    return (0, 0, 0.0, sr);
                }
            }
            (0, 0, 0.0, 0)
        }
        TrackKind::Other => (0, 0, 0.0, 0),
    }
}

/// Extracts the SPS NALU from an avcC configuration record and parses its
/// dimensions. (H265 hvcC parsing is not yet implemented, so it returns None.)
fn extract_video_dimensions(codec: CodecId, config: &[u8]) -> Option<(u32, u32)> {
    if codec != CodecId::H264 {
        return None;
    }
    // avcC layout:
    //   configurationVersion(1) profile(1) compat(1) level(1)
    //   lengthSizeMinus1(1) numSPS(1)
    //   then per SPS: 2-byte length + NALU
    if config.len() < 6 {
        return None;
    }
    let sps_count = config[5] & 0x1F;
    if sps_count == 0 || config.len() < 7 {
        return None;
    }
    let len = u16::from_be_bytes([config[6], config[7]]) as usize;
    if config.len() < 8 + len {
        return None;
    }
    let sps_nalu = &config[8..8 + len];
    let info = zlmediakit_codec::h264::H264Parser::parse_sps(sps_nalu)?;
    Some((info.width, info.height))
}

/// Extracts the audio sample rate from an AudioSpecificConfig (AAC).
fn extract_audio_sample_rate(config: &[u8]) -> Option<u32> {
    let cfg = zlmediakit_codec::aac::AacParser::parse_audio_specific_config(config)?;
    Some(cfg.sample_rate)
}

fn read_tkhd_track_id(tkhd: &[u8]) -> u32 {
    // body = version/flags(4) + creation(4) + modification(4) + track_id(4)
    if tkhd.len() >= 16 {
        u32::from_be_bytes([tkhd[12], tkhd[13], tkhd[14], tkhd[15]])
    } else {
        0
    }
}

fn read_mdhd_timescale(mdhd: &[u8]) -> u32 {
    // body = version/flags(4) + creation(4) + modification(4) + timescale(4)
    if mdhd.len() >= 16 {
        u32::from_be_bytes([mdhd[12], mdhd[13], mdhd[14], mdhd[15]])
    } else {
        90000
    }
}

fn read_mdhd_duration(mdhd: &[u8]) -> u64 {
    // body = version/flags(4) + creation(4) + modification(4) + timescale(4)
    //        + duration(4)
    if mdhd.len() >= 20 {
        u64::from_be_bytes([0, 0, 0, 0, mdhd[16], mdhd[17], mdhd[18], mdhd[19]])
    } else {
        0
    }
}

fn read_hdlr_type(hdlr: &[u8]) -> TrackKind {
    // body = version/flags(4) + pre_defined(4) + handler_type(4)
    if hdlr.len() >= 12 {
        let ht = &hdlr[8..12];
        match ht {
            b"vide" => TrackKind::Video,
            b"soun" => TrackKind::Audio,
            _ => TrackKind::Other,
        }
    } else {
        TrackKind::Other
    }
}

/// Parses stsd and returns (codec, decoder_config_record_bytes).
fn parse_stsd(stsd: &[u8], handler: TrackKind) -> Result<(CodecId, Vec<u8>)> {
    // stsd box body (from walk) = version/flags(4) + entry_count(4) + entries.
    if stsd.len() < 8 {
        bail!("truncated stsd");
    }
    let entry_count = u32::from_be_bytes([stsd[4], stsd[5], stsd[6], stsd[7]]);
    if entry_count == 0 {
        bail!("stsd has no entries");
    }
    // First sample entry starts after the 8-byte stsd header (version/flags
    // + entry_count).
    let entry = &stsd[8..];
    let boxes = BoxCursor::new(entry).walk_top_boxes()?;
    // The sample entry itself is a box (e.g. avc1) whose body contains the
    // codec-specific boxes (avcC/hvcC/esds).
    let (entry_name, sample_entry) = match boxes.iter().next() {
        Some((name, b)) => (name.clone(), *b),
        None => bail!("stsd missing sample entry"),
    };
    // fourcc is the sample entry box type (the walk key).
    let fourcc: &[u8; 4] = match entry_name.as_bytes() {
        b if b.len() == 4 => b.try_into().unwrap(),
        _ => bail!("bad sample entry name"),
    };
    match handler {
        TrackKind::Video => match fourcc {
            b"avc1" | b"avc3" => {
                let cfg = extract_avcc(sample_entry);
                Ok((CodecId::H264, cfg))
            }
            b"hvc1" | b"hev1" => {
                let cfg = extract_hvcc(sample_entry);
                Ok((CodecId::H265, cfg))
            }
            _ => bail!("unsupported video codec fourcc {:?}", fourcc),
        },
        TrackKind::Audio => match fourcc {
            b"mp4a" => {
                let cfg = extract_aac_config(sample_entry);
                Ok((CodecId::AAC, cfg))
            }
            _ => bail!("unsupported audio codec fourcc {:?}", fourcc),
        },
        TrackKind::Other => bail!("unknown track kind in stsd"),
    }
}

/// Extracts the avcC box payload (AVCDecoderConfigurationRecord) from a
/// sample entry.
fn extract_avcc(sample_entry: &[u8]) -> Vec<u8> {
    if let Some(payload) = find_child_box(sample_entry, b"avcC") {
        payload.to_vec()
    } else {
        Vec::new()
    }
}

fn extract_hvcc(sample_entry: &[u8]) -> Vec<u8> {
    if let Some(payload) = find_child_box(sample_entry, b"hvcC") {
        payload.to_vec()
    } else {
        Vec::new()
    }
}

/// Extracts the AudioSpecificConfig (ASC) from the esds box of an mp4a entry.
fn extract_aac_config(sample_entry: &[u8]) -> Vec<u8> {
    if let Some(esds) = find_child_box(sample_entry, b"esds") {
        if let Some(asc) = parse_esds_asc(&esds) {
            return asc;
        }
    }
    vec![0x12, 0x10]
}

/// Finds a child box inside a sample entry by locating its 4-char type tag and
/// reading the preceding size field. This is more robust than a strict box
/// walk because a `SampleEntry` body begins with fixed fields (reserved,
/// data_reference_index, pre_defined, etc.) rather than box-aligned data, so a
/// strict sequential walk would desynchronize before reaching the codec config
/// box (e.g. `avcC`).
fn find_child_box(data: &[u8], target: &[u8; 4]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        if &data[pos + 4..pos + 8] == target {
            let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            let payload_start = pos + 8;
            let payload_end = if size >= 8 {
                pos + size
            } else {
                // malformed size; take the remainder as payload
                data.len()
            };
            if payload_end <= data.len() {
                return Some(data[payload_start..payload_end].to_vec());
            }
        }
        pos += 1;
    }
    None
}

/// Parses the DecoderSpecificInfo (ASC) out of an esds box.
fn parse_esds_asc(esds: &[u8]) -> Option<Vec<u8>> {
    // esds body (from walk) = version/flags(4) + ES_Descriptor.
    let mut i = 4;
    if i >= esds.len() {
        return None;
    }
    // Find tag 0x05 (DecoderSpecificInfo) by shallow scan.
    while i + 2 < esds.len() {
        let tag = esds[i];
        if tag == 0x05 {
            let (len, n) = read_ber_len(&esds[i + 1..])?;
            let start = i + 1 + n;
            if start + len <= esds.len() {
                return Some(esds[start..start + len].to_vec());
            }
            return None;
        }
        if tag == 0x03 || tag == 0x04 {
            let (len, n) = read_ber_len(&esds[i + 1..])?;
            i = i + 1 + n + len;
        } else {
            break;
        }
    }
    None
}

fn read_ber_len(data: &[u8]) -> Option<(usize, usize)> {
    if data.is_empty() {
        return None;
    }
    let b = data[0];
    if b & 0x80 == 0 {
        Some((b as usize, 1))
    } else {
        let num = (b & 0x7F) as usize;
        if num + 1 > data.len() {
            return None;
        }
        let mut len = 0usize;
        for &byte in &data[1..num + 1] {
            len = (len << 8) | byte as usize;
        }
        Some((len, 1 + num))
    }
}

fn ensure_table_entries(
    box_name: &str,
    body_len: usize,
    header_len: usize,
    entry_count: usize,
    entry_size: usize,
) -> Result<()> {
    let required = entry_count
        .checked_mul(entry_size)
        .and_then(|entries| header_len.checked_add(entries))
        .context("mp4 table size overflow")?;
    if required > body_len {
        bail!("truncated {box_name}: declares {entry_count} entries");
    }
    Ok(())
}

fn parse_stts(stts: &[u8], expected_samples: usize) -> Result<Vec<u32>> {
    // body = version/flags(4) + entry_count(4) + (count, delta) entries
    if stts.len() < 8 {
        bail!("truncated stts");
    }
    let entry_count = u32::from_be_bytes([stts[4], stts[5], stts[6], stts[7]]) as usize;
    ensure_table_entries("stts", stts.len(), 8, entry_count, 8)?;
    let mut out = Vec::with_capacity(expected_samples);
    let mut pos = 8;
    for _ in 0..entry_count {
        let count =
            u32::from_be_bytes([stts[pos], stts[pos + 1], stts[pos + 2], stts[pos + 3]]) as usize;
        let delta =
            u32::from_be_bytes([stts[pos + 4], stts[pos + 5], stts[pos + 6], stts[pos + 7]]);
        let new_len = out
            .len()
            .checked_add(count)
            .context("stts sample count overflow")?;
        if new_len > expected_samples {
            bail!("stts describes more samples than stsz");
        }
        out.resize(new_len, delta);
        pos += 8;
    }
    if out.len() != expected_samples {
        bail!("stts sample count does not match stsz");
    }
    Ok(out)
}

/// Returns `(first_chunk, samples_per_chunk, sample_description_index)` entries.
/// Each entry applies from `first_chunk` through the chunk before the next entry.
fn parse_stsc(stsc: &[u8]) -> Result<Vec<(u32, usize, u32)>> {
    // body = version/flags(4) + entry_count(4) + (first_chunk, samples_per,
    // sample_desc) entries
    if stsc.len() < 8 {
        bail!("truncated stsc");
    }
    let entry_count = u32::from_be_bytes([stsc[4], stsc[5], stsc[6], stsc[7]]) as usize;
    ensure_table_entries("stsc", stsc.len(), 8, entry_count, 12)?;
    let mut entries = Vec::with_capacity(entry_count);
    let mut pos = 8;
    for _ in 0..entry_count {
        let first_chunk =
            u32::from_be_bytes([stsc[pos], stsc[pos + 1], stsc[pos + 2], stsc[pos + 3]]);
        let samples_per_chunk =
            u32::from_be_bytes([stsc[pos + 4], stsc[pos + 5], stsc[pos + 6], stsc[pos + 7]])
                as usize;
        let sample_desc =
            u32::from_be_bytes([stsc[pos + 8], stsc[pos + 9], stsc[pos + 10], stsc[pos + 11]]);
        entries.push((first_chunk, samples_per_chunk, sample_desc));
        pos += 12;
    }
    Ok(entries)
}

fn parse_stsz(stsz: &[u8], file_len: usize) -> Result<Vec<usize>> {
    // body = version/flags(4) + sample_size(4) + sample_count(4) + [sizes]
    if stsz.len() < 12 {
        bail!("truncated stsz");
    }
    let sample_size = u32::from_be_bytes([stsz[4], stsz[5], stsz[6], stsz[7]]);
    let sample_count = u32::from_be_bytes([stsz[8], stsz[9], stsz[10], stsz[11]]) as usize;
    if sample_size != 0 {
        let sample_size = sample_size as usize;
        if sample_count > file_len / sample_size {
            bail!("stsz sample count exceeds the containing file size");
        }
        return Ok(vec![sample_size; sample_count]);
    }
    ensure_table_entries("stsz", stsz.len(), 12, sample_count, 4)?;
    let mut out = Vec::with_capacity(sample_count);
    let mut pos = 12;
    for _ in 0..sample_count {
        let s =
            u32::from_be_bytes([stsz[pos], stsz[pos + 1], stsz[pos + 2], stsz[pos + 3]]) as usize;
        out.push(s);
        pos += 4;
    }
    Ok(out)
}

fn parse_stco(stco: &[u8]) -> Result<Vec<u64>> {
    // body = version/flags(4) + entry_count(4) + offsets
    if stco.len() < 8 {
        bail!("truncated stco");
    }
    let count = u32::from_be_bytes([stco[4], stco[5], stco[6], stco[7]]) as usize;
    ensure_table_entries("stco", stco.len(), 8, count, 4)?;
    let mut out = Vec::with_capacity(count);
    let mut pos = 8;
    for _ in 0..count {
        let o = u32::from_be_bytes([stco[pos], stco[pos + 1], stco[pos + 2], stco[pos + 3]]) as u64;
        out.push(o);
        pos += 4;
    }
    Ok(out)
}

fn parse_co64(co64: &[u8]) -> Result<Vec<u64>> {
    // body = version/flags(4) + entry_count(4) + 64-bit offsets
    if co64.len() < 8 {
        bail!("truncated co64");
    }
    let count = u32::from_be_bytes([co64[4], co64[5], co64[6], co64[7]]) as usize;
    ensure_table_entries("co64", co64.len(), 8, count, 8)?;
    let mut out = Vec::with_capacity(count);
    let mut pos = 8;
    for _ in 0..count {
        let o = u64::from_be_bytes([
            co64[pos],
            co64[pos + 1],
            co64[pos + 2],
            co64[pos + 3],
            co64[pos + 4],
            co64[pos + 5],
            co64[pos + 6],
            co64[pos + 7],
        ]);
        out.push(o);
        pos += 8;
    }
    Ok(out)
}

fn parse_stss(stss: &[u8]) -> Result<Vec<u32>> {
    // body = version/flags(4) + entry_count(4) + sync sample numbers
    if stss.len() < 8 {
        bail!("truncated stss");
    }
    let count = u32::from_be_bytes([stss[4], stss[5], stss[6], stss[7]]) as usize;
    ensure_table_entries("stss", stss.len(), 8, count, 4)?;
    let mut out = Vec::with_capacity(count);
    let mut pos = 8;
    for _ in 0..count {
        let n = u32::from_be_bytes([stss[pos], stss[pos + 1], stss[pos + 2], stss[pos + 3]]);
        out.push(n);
        pos += 4;
    }
    Ok(out)
}

// ── FLV-style frame reconstruction ─────────────────────────────────────

/// Builds the FLV config frame payload (decoder configuration record) for a
/// track. Mirrors the framing used by the publisher path.
fn build_flv_config(track: &DemuxedTrack, config: &[u8]) -> Vec<u8> {
    match track.handler {
        TrackKind::Video => match track.codec {
            CodecId::H264 => {
                let mut v = vec![0x17, 0x00, 0x00, 0x00, 0x00];
                v.extend_from_slice(config);
                v
            }
            CodecId::H265 => {
                let mut v = vec![0x1c, 0x00, 0x00, 0x00, 0x00];
                v.extend_from_slice(config);
                v
            }
            _ => config.to_vec(),
        },
        TrackKind::Audio => {
            // AAC sequence header: 0xAF 0x00 + AudioSpecificConfig
            let mut v = vec![0xAF, 0x00];
            v.extend_from_slice(config);
            v
        }
        TrackKind::Other => config.to_vec(),
    }
}

/// Builds the FLV data frame payload for a single sample.
fn build_flv_sample(track: &DemuxedTrack, sample: &[u8], is_sync: bool) -> Vec<u8> {
    match track.handler {
        TrackKind::Video => match track.codec {
            CodecId::H264 => {
                let mut v = vec![0x17, 0x01, 0x00, 0x00, 0x00];
                v.extend_from_slice(sample);
                v
            }
            CodecId::H265 => {
                let tag = if is_sync { 0x1c } else { 0x2c };
                let mut v = vec![tag, 0x01, 0x00, 0x00, 0x00];
                v.extend_from_slice(sample);
                v
            }
            _ => sample.to_vec(),
        },
        TrackKind::Audio => {
            // AAC raw frame: 0xAF 0x01 + raw AAC data
            let mut v = vec![0xAF, 0x01];
            v.extend_from_slice(sample);
            v
        }
        TrackKind::Other => sample.to_vec(),
    }
}

// Keep FrameType referenced (used by callers converting to MediaFrame).
#[allow(dead_code)]
fn _frame_type_marker(_: FrameType) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::muxer::Mp4Muxer;
    use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};

    fn build_test_mp4() -> Vec<u8> {
        let mut muxer = Mp4Muxer::new();
        // H264 config (SPS/PPS as FLV AVC sequence header)
        let mut cfg = MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::from(vec![
                0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x05, 0x67,
                0x42, 0x00, 0x1e, 0xa6, 0x01, 0x68, 0xce, 0x3c, 0x80,
            ]),
            true,
        );
        cfg.config_frame = true;
        muxer.push_frame(&cfg);
        // 3 video frames at 0/33/66 ms
        for (i, ts) in [0u32, 33, 66].iter().enumerate() {
            let is_key = i == 0;
            let mut data = vec![if is_key { 0x17 } else { 0x27 }, 0x01, 0x00, 0x00, 0x00];
            data.extend_from_slice(&[0xAA + i as u8; 4]);
            let f = MediaFrame::new_video(
                0,
                CodecId::H264,
                *ts,
                *ts as u64,
                *ts as u64,
                Bytes::from(data),
                is_key,
            );
            muxer.push_frame(&f);
        }
        // AAC config + 2 frames
        let mut acfg = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            0,
            0,
            0,
            Bytes::from(vec![0xAF, 0x00, 0x12, 0x10]),
        );
        acfg.config_frame = true;
        muxer.push_frame(&acfg);
        for ts in [0u32, 23] {
            let f = MediaFrame::new_audio(
                1,
                CodecId::AAC,
                ts,
                ts as u64,
                ts as u64,
                Bytes::from(vec![0xAF, 0x01, 0x01, 0x02, 0x03]),
            );
            muxer.push_frame(&f);
        }

        muxer.finalize().to_vec()
    }

    #[test]
    fn demux_roundtrip_counts_and_codecs() {
        let mp4 = build_test_mp4();
        assert!(mp4.len() > 100);
        let demuxer = Mp4Demuxer::from_bytes(&mp4).expect("parse mp4");
        let tracks = demuxer.tracks();
        assert_eq!(tracks.len(), 2, "expected video + audio tracks");

        let video = tracks
            .iter()
            .find(|t| t.handler == TrackKind::Video)
            .unwrap();
        assert_eq!(video.codec, CodecId::H264);
        assert_eq!(video.sample_count, 3);
        // config record present
        assert!(!video.config.is_empty());

        let audio = tracks
            .iter()
            .find(|t| t.handler == TrackKind::Audio)
            .unwrap();
        assert_eq!(audio.codec, CodecId::AAC);
        assert_eq!(audio.sample_count, 2);

        // Verify sample timestamps are monotonically increasing in ms.
        let vs = demuxer.samples_for(video.track_id).unwrap();
        assert_eq!(vs.len(), 3);
        assert!(vs[0].pts_ms <= vs[1].pts_ms);
        assert!(vs[1].pts_ms <= vs[2].pts_ms);
        assert!(vs[0].is_sync, "first video frame should be a key frame");
    }

    #[test]
    fn demux_to_media_frames_rebuilds_flv() {
        let mp4 = build_test_mp4();
        let demuxer = Mp4Demuxer::from_bytes(&mp4).unwrap();
        let frames = demuxer.to_media_frames();
        // 1 video config + 3 video + 1 audio config + 2 audio = 7
        assert_eq!(frames.len(), 7);
        assert!(frames
            .iter()
            .any(|f| f.config_frame && f.frame_type == FrameType::Video));
        assert!(frames
            .iter()
            .any(|f| f.config_frame && f.frame_type == FrameType::Audio));
        // First video data frame uses FLV key-frame tag 0x17.
        let first_video = frames
            .iter()
            .find(|f| f.frame_type == FrameType::Video && !f.config_frame)
            .unwrap();
        assert_eq!(first_video.data[0], 0x17);
    }

    #[test]
    fn demux_interleaved_frames_are_pts_ordered() {
        let mp4 = build_test_mp4();
        let demuxer = Mp4Demuxer::from_bytes(&mp4).unwrap();
        let frames = demuxer.interleaved_media_frames();
        // 1 video config + 1 audio config + 3 video + 2 audio = 7
        assert_eq!(frames.len(), 7);
        // Config frames come first (video then audio).
        assert!(frames[0].config_frame && frames[0].frame_type == FrameType::Video);
        assert!(frames[1].config_frame && frames[1].frame_type == FrameType::Audio);

        // Data frames must be interleaved by ascending pts.
        let data_pts: Vec<u32> = frames
            .iter()
            .filter(|f| !f.config_frame)
            .map(|f| f.timestamp)
            .collect();
        let mut sorted = data_pts.clone();
        sorted.sort();
        assert_eq!(
            data_pts, sorted,
            "data frames must be in playback (pts) order"
        );

        // Audio track params should be derived from the decoder config.
        let tracks = demuxer.tracks();
        let audio = tracks
            .iter()
            .find(|t| t.handler == TrackKind::Audio)
            .unwrap();
        assert!(audio.sample_rate > 0, "audio sample rate should be parsed");
    }

    #[test]
    fn fuzz_smoke_mp4_demuxer_never_panics() {
        let valid = build_test_mp4();
        // MP4 parsing walks several nested sample tables. Sample at most 512
        // truncation/mutation points so this remains a bounded CI smoke test.
        let stride = (valid.len() / 512).max(1);
        for len in (0..valid.len()).step_by(stride) {
            let _ = Mp4Demuxer::from_bytes(&valid[..len]);
        }
        for index in (0..valid.len()).step_by(stride) {
            let mut mutated = valid.clone();
            mutated[index] ^= 0xff;
            let _ = Mp4Demuxer::from_bytes(&mutated);
        }
    }
}
