//! Fragmented MP4 (fMP4 / CMAF) muxer for DASH streaming.
//!
//! Generates init segments (ftyp+moov) and media segments
//! (styp+moof+mdat) suitable for DASH/HLS CMAF delivery.

use bytes::{BufMut, Bytes, BytesMut};
use std::collections::VecDeque;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};

/// A single fMP4 segment ready for delivery.
#[derive(Debug, Clone)]
pub struct Fmp4Segment {
    /// `true` if this is the init segment, `false` for media segments.
    pub is_init: bool,
    /// Sequence number (0 for init, incremental for media).
    pub sequence: u64,
    /// Segment data.
    pub data: Bytes,
    /// Start timestamp in timescale units.
    pub start_time: u64,
    /// Duration in timescale units.
    pub duration: u64,
}

/// Fragmented MP4 muxer for live streaming.
///
/// Call `push_frame()` for each incoming frame.
/// Call `flush_segment()` periodically (e.g. every N seconds) to
/// produce a media segment.
/// Call `init_segment()` once to get the initialization segment.
pub struct Fmp4Muxer {
    video_timescale: u32,
    video_codec: Option<CodecId>,
    audio_codec: Option<CodecId>,
    video_config: Option<Bytes>,
    audio_config: Option<Bytes>,
    width: u32,
    height: u32,
    audio_sample_rate: u32,
    audio_channels: u32,

    video_samples: VecDeque<Fmp4Sample>,
    audio_samples: VecDeque<Fmp4Sample>,
    base_timestamp: Option<u32>,
    segment_seq: u64,
    segment_start_time: u64,
    total_duration: u64,           // in timescale units
    largest_video_sample: usize,
    largest_audio_sample: usize,
}

#[derive(Debug, Clone)]
struct Fmp4Sample {
    data: Bytes,
    #[allow(dead_code)]
    timestamp: u32,
    duration: u32,        // in timescale units
    is_sync: bool,
}

impl Fmp4Muxer {
    pub fn new(video_timescale: u32) -> Self {
        Self {
            video_timescale,
            video_codec: None,
            audio_codec: None,
            video_config: None,
            audio_config: None,
            width: 640,
            height: 480,
            audio_sample_rate: 44100,
            audio_channels: 2,
            video_samples: VecDeque::new(),
            audio_samples: VecDeque::new(),
            base_timestamp: None,
            segment_seq: 0,
            segment_start_time: 0,
            total_duration: 0,
            largest_video_sample: 0,
            largest_audio_sample: 0,
        }
    }

    /// Feeds a frame into the muxer. Config frames are captured for
    /// the init segment but not written as samples.
    pub fn push_frame(&mut self, frame: &MediaFrame) {
        if self.base_timestamp.is_none() {
            self.base_timestamp = Some(frame.timestamp);
        }

        if frame.config_frame {
            match frame.frame_type {
                zlmediakit_core::media_frame::FrameType::Video => {
                    self.video_codec = Some(frame.codec);
                    self.video_config = Some(frame.data.clone());
                }
                zlmediakit_core::media_frame::FrameType::Audio => {
                    self.audio_codec = Some(frame.codec);
                    self.audio_config = Some(frame.data.clone());
                }
                _ => {}
            }
            return;
        }

        let rel_ts = frame.timestamp.saturating_sub(self.base_timestamp.unwrap_or(0));
        let duration = self.video_timescale / 25; // default 25fps fallback

        let sample = Fmp4Sample {
            data: frame.data.clone(),
            timestamp: rel_ts,
            duration,
            is_sync: frame.key_frame,
        };

        let size = sample.data.len();
        match frame.frame_type {
            zlmediakit_core::media_frame::FrameType::Video => {
                self.largest_video_sample = self.largest_video_sample.max(size);
                self.video_samples.push_back(sample);
            }
            zlmediakit_core::media_frame::FrameType::Audio => {
                self.largest_audio_sample = self.largest_audio_sample.max(size);
                self.audio_samples.push_back(sample);
            }
            _ => {}
        }

        // Update total duration
        let ts_in_scale = (rel_ts as u64) * (self.video_timescale as u64) / 1000;
        let end = ts_in_scale + duration as u64;
        self.total_duration = self.total_duration.max(end);
    }

    /// Returns the initialization segment. Call once before any media segments.
    pub fn init_segment(&self) -> Fmp4Segment {
        let data = self.build_init_segment();
        Fmp4Segment {
            is_init: true,
            sequence: 0,
            data: data.freeze(),
            start_time: 0,
            duration: 0,
        }
    }

    /// Flushes accumulated samples into a media segment and returns it.
    /// Returns `None` if there are no samples.
    pub fn flush_segment(&mut self) -> Option<Fmp4Segment> {
        if self.video_samples.is_empty() && self.audio_samples.is_empty() {
            return None;
        }

        let seq = self.segment_seq;
        self.segment_seq += 1;

        let start_time = self.segment_start_time;
        let segment_duration = self.total_duration.saturating_sub(start_time);

        let video: Vec<&Fmp4Sample> = self.video_samples.iter().collect();
        let audio: Vec<&Fmp4Sample> = self.audio_samples.iter().collect();

        let data = self.build_media_segment(seq, start_time, &video, &audio);

        // Reset for next segment
        self.segment_start_time = self.total_duration;
        self.video_samples.clear();
        self.audio_samples.clear();

        Some(Fmp4Segment {
            is_init: false,
            sequence: seq,
            data: data.freeze(),
            start_time,
            duration: segment_duration,
        })
    }

    pub fn has_samples(&self) -> bool {
        !self.video_samples.is_empty() || !self.audio_samples.is_empty()
    }

    pub fn video_codec(&self) -> Option<CodecId> {
        self.video_codec
    }

    pub fn audio_codec(&self) -> Option<CodecId> {
        self.audio_codec
    }

    // ── init segment (ftyp + moov) ────────────────────────

    fn build_init_segment(&self) -> BytesMut {
        let mut buf = BytesMut::new();
        self.write_ftyp(&mut buf);
        self.write_moov(&mut buf);
        buf
    }

    fn write_ftyp(&self, buf: &mut BytesMut) {
        // CMAF-compatible brands
        let brands = b"iso6mp41cmfc";
        // For H.265, use "hvc1" compatible; for H.264 use "avc1"
        let major = match self.video_codec {
            Some(CodecId::H265) => b"iso6",
            _ => b"isom",
        };
        let mut data = BytesMut::new();
        data.extend_from_slice(major);
        data.put_u32(0x200); // minor version
        data.extend_from_slice(brands);
        let ftyp = make_box(b"ftyp", &data);
        buf.extend_from_slice(&ftyp);
    }

    fn write_moov(&self, buf: &mut BytesMut) {
        let mut moov_data = BytesMut::new();

        // mvhd
        let mut mvhd = BytesMut::new();
        mvhd.put_u32(0); // version+flags
        mvhd.put_u32(0); // creation time
        mvhd.put_u32(0); // modification time
        mvhd.put_u32(1000); // timescale
        mvhd.put_u32(0); // duration (0 for live)
        mvhd.put_u32(0x00010000); // rate 1.0
        mvhd.put_u16(0x0100); // volume 1.0
        mvhd.put_u16(0); // reserved
        mvhd.extend_from_slice(&[0u8; 8]); // reserved
        mvhd.put_u32(0x00010000); // matrix[0]
        mvhd.put_u32(0);
        mvhd.put_u32(0);
        mvhd.put_u32(0);
        mvhd.put_u32(0x00010000);
        mvhd.put_u32(0);
        mvhd.put_u32(0);
        mvhd.put_u32(0);
        mvhd.put_u32(0x40000000); // matrix[8]
        mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
        mvhd.put_u32(2); // next_track_id
        moov_data.extend_from_slice(&make_box(b"mvhd", &mvhd));

        // Video track
        if let Some(ref vc) = self.video_codec {
            if let Some(ref vcfg) = self.video_config {
                self.write_trak(
                    &mut moov_data,
                    1,
                    vc,
                    vcfg,
                    self.width,
                    self.height,
                    self.video_timescale,
                    self.video_samples.iter().map(|s| s.data.len() as u32).collect(),
                );
            }
        }

        // Audio track
        if let Some(ref ac) = self.audio_codec {
            if let Some(ref acfg) = self.audio_config {
                self.write_trak(
                    &mut moov_data,
                    2,
                    ac,
                    acfg,
                    0,
                    0,
                    self.audio_sample_rate,
                    self.audio_samples.iter().map(|s| s.data.len() as u32).collect(),
                );
            }
        }

        buf.extend_from_slice(&make_box(b"moov", &moov_data));
    }

    fn write_trak(
        &self,
        buf: &mut BytesMut,
        track_id: u32,
        codec: &CodecId,
        config: &[u8],
        width: u32,
        height: u32,
        timescale: u32,
        _sample_sizes: Vec<u32>,
    ) {
        let is_video = matches!(codec, CodecId::H264 | CodecId::H265);
        let mut trak = BytesMut::new();

        // tkhd
        {
            let mut tkhd = BytesMut::new();
            let flags = 7u32; // track_enabled | in_movie | in_preview for both
            tkhd.put_u32(flags);
            tkhd.put_u32(0); // creation
            tkhd.put_u32(0); // modification
            tkhd.put_u32(track_id);
            tkhd.put_u32(0); // reserved
            tkhd.put_u32(0); // duration (0 for fragmented)
            tkhd.extend_from_slice(&[0u8; 8]); // reserved
            tkhd.put_u16(0); // layer
            tkhd.put_u16(0); // alternate group
            if is_video {
                tkhd.put_u16(0x0100); // volume
            } else {
                tkhd.put_u16(0x0100);
            }
            tkhd.put_u16(0); // reserved
            // identity matrix (9 fixed-point 16.16 values = 36 bytes):
            // [ 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0 ]
            tkhd.put_u32(0x00010000); tkhd.put_u32(0); tkhd.put_u32(0);
            tkhd.put_u32(0); tkhd.put_u32(0x00010000); tkhd.put_u32(0);
            tkhd.put_u32(0); tkhd.put_u32(0); tkhd.put_u32(0x40000000);
            tkhd.put_u32((width as u32) << 16); // width fixed-point
            tkhd.put_u32((height as u32) << 16); // height fixed-point
            trak.extend_from_slice(&make_box(b"tkhd", &tkhd));
        }

        // mdia
        {
            let mut mdia = BytesMut::new();

            // mdhd
            {
                let mut mdhd = BytesMut::new();
                mdhd.put_u32(0); // version+flags
                mdhd.put_u32(0); // creation
                mdhd.put_u32(0); // modification
                mdhd.put_u32(timescale);
                mdhd.put_u32(0); // duration (0 for fragmented)
                mdhd.put_u16(0x55C4); // language = und
                mdhd.put_u16(0); // pre-defined
                mdia.extend_from_slice(&make_box(b"mdhd", &mdhd));
            }

            // hdlr
            {
                let mut hdlr = BytesMut::new();
                hdlr.put_u32(0); // version+flags
                hdlr.put_u32(0); // pre-defined
                if is_video {
                    hdlr.extend_from_slice(b"vide");
                } else {
                    hdlr.extend_from_slice(b"soun");
                }
                hdlr.extend_from_slice(&[0u8; 12]); // reserved
                hdlr.extend_from_slice(b"zlmediakit-rs\0");
                mdia.extend_from_slice(&make_box(b"hdlr", &hdlr));
            }

            // minf
            {
                let mut minf = BytesMut::new();

                if is_video {
                    let mut vmhd = BytesMut::new();
                    vmhd.put_u32(1); // version+flags
                    vmhd.put_u16(0); // graphics mode
                    vmhd.extend_from_slice(&[0u8; 6]); // opcolor
                    minf.extend_from_slice(&make_box(b"vmhd", &vmhd));
                } else {
                    let mut smhd = BytesMut::new();
                    smhd.put_u32(0);
                    smhd.put_u16(0); // balance
                    smhd.put_u16(0); // reserved
                    minf.extend_from_slice(&make_box(b"smhd", &smhd));
                }

                // dinf
                {
                    let mut dref = BytesMut::new();
                    dref.put_u32(0); // version+flags
                    dref.put_u32(1); // entry_count
                    let url = make_box(b"url ", &0u32.to_be_bytes());
                    dref.extend_from_slice(&url);
                    minf.extend_from_slice(&make_box(b"dinf", &make_box(b"dref", &dref)));
                }

                // stbl (with empty tables — data goes in fragments)
                {
                    let mut stbl = BytesMut::new();

                    // stsd
                    {
                        let mut stsd = BytesMut::new();
                        stsd.put_u32(0);
                        stsd.put_u32(1); // entry count

                        match codec {
                            CodecId::H264 => {
                                let entry = build_avc1_entry(config, width, height);
                                stsd.extend_from_slice(&entry);
                            }
                            CodecId::H265 => {
                                let entry = build_hvc1_entry(config, width, height);
                                stsd.extend_from_slice(&entry);
                            }
                            CodecId::AAC => {
                                let entry = build_mp4a_entry(config, timescale, self.audio_channels as u32);
                                stsd.extend_from_slice(&entry);
                            }
                            CodecId::VP8 | CodecId::VP9 | CodecId::AV1 => {
                                let fourcc = match codec {
                                    CodecId::VP8 => b"vp08",
                                    CodecId::VP9 => b"vp09",
                                    _ => b"av01",
                                };
                                let entry = build_vp_entry(fourcc, config, width, height);
                                stsd.extend_from_slice(&entry);
                            }
                            _ => {}
                        }

                        stbl.extend_from_slice(&make_box(b"stsd", &stsd));
                    }

                    // stts (empty — samples in fragments)
                    stbl.extend_from_slice(&make_box(b"stts", &0u32.to_be_bytes()));
                    // stsc (empty)
                    stbl.extend_from_slice(&make_box(b"stsc", &0u32.to_be_bytes()));
                    // stsz (empty)
                    stbl.extend_from_slice(&make_box(b"stsz", &[0u8; 8])); // sample_size=0 + count=0
                    // stco (empty)
                    stbl.extend_from_slice(&make_box(b"stco", &0u32.to_be_bytes()));

                    minf.extend_from_slice(&make_box(b"stbl", &stbl));
                }

                mdia.extend_from_slice(&make_box(b"minf", &minf));
            }

            trak.extend_from_slice(&make_box(b"mdia", &mdia));
        }

        buf.extend_from_slice(&make_box(b"trak", &trak));
    }

    // ── media segment (styp + moof + mdat) ────────────────

    fn build_media_segment(
        &self,
        seq: u64,
        start_time: u64,
        video: &[&Fmp4Sample],
        audio: &[&Fmp4Sample],
    ) -> BytesMut {
        let mut buf = BytesMut::new();

        // styp
        {
            let mut styp_data = BytesMut::new();
            styp_data.extend_from_slice(b"msdh"); // MPEG-DASH segment
            styp_data.extend_from_slice(b"msix");
            styp_data.extend_from_slice(b"iso6");
            styp_data.extend_from_slice(b"cmfc");
            buf.extend_from_slice(&make_box(b"styp", &styp_data));
        }

        // Reserve space for moof (we'll write it after building mdat)
        // We need to know sample offsets...

        // Build mdat first
        let mdat_data = self.build_mdat(video, audio);
        let moof_offset = 8 + mdat_data.len() as u32; // mdat header + data

        // Now build moof knowing the absolute offsets
        let moof = self.build_moof(seq, start_time, video, audio, moof_offset);

        buf.extend_from_slice(&moof);
        buf.extend_from_slice(&make_box(b"mdat", &mdat_data));

        buf
    }

    fn build_moof(
        &self,
        seq: u64,
        start_time: u64,
        video: &[&Fmp4Sample],
        audio: &[&Fmp4Sample],
        mdat_offset: u32,
    ) -> BytesMut {
        let mut moof = BytesMut::new();

        // mfhd
        {
            let mut mfhd = BytesMut::new();
            mfhd.put_u32(0);
            mfhd.put_u32(seq as u32);
            moof.extend_from_slice(&make_box(b"mfhd", &mfhd));
        }

        // Track 1 — video
        if !video.is_empty() {
            let mut accum_offset: u32 = 0;
            let traf = self.build_traf(1, start_time, video, &mut accum_offset, mdat_offset);
            moof.extend_from_slice(&traf);
        }

        // Track 2 — audio
        if !audio.is_empty() {
            let mut accum_offset: u32 = 0;
            let traf = self.build_traf(2, start_time, audio, &mut accum_offset, mdat_offset);
            moof.extend_from_slice(&traf);
        }

        make_box(b"moof", &moof)
    }

    fn build_traf(
        &self,
        track_id: u32,
        start_time: u64,
        samples: &[&Fmp4Sample],
        accum_offset: &mut u32,
        _mdat_base: u32,
    ) -> BytesMut {
        let mut traf = BytesMut::new();
        let sample_count = samples.len() as u32;

        // tfhd
        {
            let mut tfhd = BytesMut::new();
            let tf_flags = 0x020000u32; // default-base-is-moof
            tfhd.put_u32(tf_flags);
            tfhd.put_u32(track_id);
            traf.extend_from_slice(&make_box(b"tfhd", &tfhd));
        }

        // tfdt
        {
            let mut tfdt = BytesMut::new();
            tfdt.put_u32(1 << 24); // version 1
            tfdt.put_u64(start_time);
            traf.extend_from_slice(&make_box(b"tfdt", &tfdt));
        }

        // trun
        {
            let mut trun = BytesMut::new();
            let trun_flags = 0x000101u32; // data-offset | sample-duration | sample-size | sample-flags
            trun.put_u32(trun_flags);
            trun.put_u32(sample_count);
            trun.put_u32(*accum_offset + 8); // +8 for mdat box header

            for s in samples {
                trun.put_u32(s.duration);
                trun.put_u32(s.data.len() as u32);
                let flags = if s.is_sync { 0x02000000u32 } else { 0x01010000u32 };
                trun.put_u32(flags);
                *accum_offset += s.data.len() as u32;
            }

            traf.extend_from_slice(&make_box(b"trun", &trun));
        }

        make_box(b"traf", &traf)
    }

    fn build_mdat(
        &self,
        video: &[&Fmp4Sample],
        audio: &[&Fmp4Sample],
    ) -> BytesMut {
        let mut mdat = BytesMut::new();
        for s in video {
            mdat.extend_from_slice(&s.data);
        }
        for s in audio {
            mdat.extend_from_slice(&s.data);
        }
        mdat
    }
}

// ── box helpers ──────────────────────────────────────────────────────

fn make_box(box_type: &[u8; 4], data: &[u8]) -> BytesMut {
    let size = 8 + data.len() as u32;
    let mut buf = BytesMut::with_capacity(size as usize);
    buf.put_u32(size);
    buf.extend_from_slice(box_type);
    buf.extend_from_slice(data);
    buf
}

// ── codec-specific sample entries ────────────────────────────────────

fn build_avc1_entry(config: &[u8], width: u32, height: u32) -> BytesMut {
    // AVCDecoderConfigurationRecord is directly in the config from MediaFrame
    let avcc = if config.len() >= 5 {
        // Config is FLV-style: [codec_id|avc_packet_type=0|comp_time|AVCDecConfigRecord...]
        &config[5..]
    } else {
        config
    };

    let mut entry = BytesMut::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.put_u16(1); // data-reference-index
    entry.put_u16(0); // pre-defined
    entry.put_u16(0); // reserved
    entry.extend_from_slice(&[0u8; 12]); // pre-defined
    entry.put_u16(width as u16);
    entry.put_u16(height as u16);
    entry.put_u32(0x00480000); // horiz resolution 72dpi
    entry.put_u32(0x00480000); // vert resolution 72dpi
    entry.put_u32(0); // reserved
    entry.put_u16(1); // frame count
    entry.extend_from_slice(b"AVC Coding\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    entry.put_u16(24); // depth
    entry.put_i16(-1); // pre-defined
    // avcC box
    entry.extend_from_slice(&make_box(b"avcC", avcc));

    let size = 8 + entry.len() as u32;
    let mut result = BytesMut::with_capacity(size as usize);
    result.put_u32(size);
    result.extend_from_slice(b"avc1");
    result.extend_from_slice(&entry);
    result
}

fn build_hvc1_entry(config: &[u8], width: u32, height: u32) -> BytesMut {
    let hvcc = if config.len() >= 5 { &config[5..] } else { config };

    let mut entry = BytesMut::new();
    entry.extend_from_slice(&[0u8; 6]);
    entry.put_u16(1);
    entry.extend_from_slice(&[0u8; 32]);
    entry.put_u16(width as u16);
    entry.put_u16(height as u16);
    entry.put_u32(0x00480000);
    entry.put_u32(0x00480000);
    entry.put_u32(0);
    entry.put_u16(1);
    entry.extend_from_slice(b"HEVC Coding\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    entry.put_u16(24);
    entry.put_i16(-1);
    entry.extend_from_slice(&make_box(b"hvcC", hvcc));

    let size = 8 + entry.len() as u32;
    let mut result = BytesMut::with_capacity(size as usize);
    result.put_u32(size);
    result.extend_from_slice(b"hvc1");
    result.extend_from_slice(&entry);
    result
}

fn build_mp4a_entry(config: &[u8], sample_rate: u32, channels: u32) -> BytesMut {
    let aac_cfg = if config.len() >= 2 && config[0] == 0xAF {
        // FLV audio config: [0xAF, 0x00, AudioSpecificConfig...]
        if config.len() > 2 { &config[2..] } else { config }
    } else {
        config
    };

    // Build ESDS box
    let mut esds_data = BytesMut::new();
    esds_data.put_u32(0); // version+flags
    // ES_Descriptor
    esds_data.put_u8(0x03); // tag
    esds_data.put_u8(0x19); // length (placeholder)
    esds_data.put_u16(1); // ES_ID
    esds_data.put_u8(0x00); // flags
    // DecoderConfigDescriptor
    esds_data.put_u8(0x04);
    esds_data.put_u8(0x11); // length
    esds_data.put_u8(0x40); // object type = AAC
    esds_data.put_u8(0x15); // stream type
    esds_data.extend_from_slice(&[0u8; 3]); // buffer size
    esds_data.put_u32(0x0001F400); // max bitrate
    esds_data.put_u32(0x0001F400); // avg bitrate
    // DecoderSpecificInfo
    esds_data.put_u8(0x05);
    esds_data.put_u8(aac_cfg.len() as u8);
    esds_data.extend_from_slice(aac_cfg);
    // SLConfigDescriptor
    esds_data.put_u8(0x06);
    esds_data.put_u8(0x01);
    esds_data.put_u8(0x02);

    let esds_box = make_box(b"esds", &esds_data);

    let mut entry = BytesMut::new();
    entry.extend_from_slice(&[0u8; 6]);
    entry.put_u16(1);
    entry.extend_from_slice(&[0u8; 8]);
    entry.put_u16(channels as u16);
    entry.put_u16(16); // sample size
    entry.put_u16(0); // pre-defined
    entry.put_u16(0); // reserved
    entry.put_u32(sample_rate << 16); // sample rate fixed-point
    entry.extend_from_slice(&esds_box);

    let size = 8 + entry.len() as u32;
    let mut result = BytesMut::with_capacity(size as usize);
    result.put_u32(size);
    result.extend_from_slice(b"mp4a");
    result.extend_from_slice(&entry);
    result
}

fn build_vp_entry(fourcc: &[u8; 4], config: &[u8], width: u32, height: u32) -> BytesMut {
    let vpcc = config;

    let mut entry = BytesMut::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.put_u16(1); // data_reference_index
    entry.put_u16(0); // version
    entry.put_u16(0); // revision
    entry.extend_from_slice(b"google\0\0"); // vendor
    entry.put_u16(width as u16);
    entry.put_u16(height as u16);
    entry.put_u32(0x00480000); // horiz resolution
    entry.put_u32(0x00480000); // vert resolution
    entry.put_u32(0); // reserved
    entry.put_u16(1); // frame count
    entry.extend_from_slice(b"VP Coding\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    entry.put_u16(24); // depth
    entry.put_i16(-1); // color table
    // vpcC box (VP Codec Configuration)
    entry.extend_from_slice(&make_box(b"vpcC", vpcc));

    let size = 8 + entry.len() as u32;
    let mut result = BytesMut::with_capacity(size as usize);
    result.put_u32(size);
    result.extend_from_slice(fourcc);
    result.extend_from_slice(&entry);
    result
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_segment_produces_valid_boxes() {
        let mut muxer = Fmp4Muxer::new(90000);

        // Feed config frames
        let h264_config = vec![
            0x17, 0x00, 0x00, 0x00, 0x00, // FLV header
            0x01, 0x64, 0x00, 0x0a, 0xFF, 0xE1, 0x00, 0x04,
            0x67, 0x64, 0x00, 0x0a, 0x01, 0x00, 0x04,
            0x68, 0xee, 0x3c, 0x80,
        ];
        let frame = make_config_frame(CodecId::H264, &h264_config, true);
        muxer.push_frame(&frame);

        let init = muxer.init_segment();
        assert!(init.is_init);
        assert!(!init.data.is_empty());

        // Should contain ftyp and moov
        let data = &init.data;
        assert!(data.windows(4).any(|w| w == b"ftyp"), "init must have ftyp");
        assert!(data.windows(4).any(|w| w == b"moov"), "init must have moov");
    }

    #[test]
    fn empty_flush_returns_none() {
        let mut muxer = Fmp4Muxer::new(90000);
        assert!(muxer.flush_segment().is_none());
    }

    #[test]
    fn media_segment_has_moof_and_mdat() {
        let mut muxer = Fmp4Muxer::new(90000);

        // Config
        let cfg = make_config_frame(CodecId::H264, &h264_config_bytes(), true);
        muxer.push_frame(&cfg);

        // Video samples
        let sample = make_media_frame(CodecId::H264, &[0x00, 0x00, 0x00, 0x01, 0x21, 0x01], 100, true);
        muxer.push_frame(&sample);

        let seg = muxer.flush_segment().unwrap();
        assert!(!seg.is_init);
        assert!(seg.data.windows(4).any(|w| w == b"moof"), "segment must have moof");
        assert!(seg.data.windows(4).any(|w| w == b"mdat"), "segment must have mdat");
        assert!(seg.duration > 0);
    }

    fn h264_config_bytes() -> Vec<u8> {
        vec![
            0x17, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x64, 0x00, 0x0a, 0xFF, 0xE1, 0x00, 0x04,
            0x67, 0x64, 0x00, 0x0a, 0x01, 0x00, 0x04,
            0x68, 0xee, 0x3c, 0x80,
        ]
    }

    fn make_config_frame(codec: CodecId, data: &[u8], video: bool) -> MediaFrame {
        let mut f = if video {
            MediaFrame::new_video(0, codec, 0, 0, 0, Bytes::copy_from_slice(data), false)
        } else {
            MediaFrame::new_audio(0, codec, 0, 0, 0, Bytes::copy_from_slice(data))
        };
        f.config_frame = true;
        f
    }

    fn make_media_frame(codec: CodecId, data: &[u8], timestamp: u32, key: bool) -> MediaFrame {
        MediaFrame::new_video(0, codec, timestamp, timestamp as u64, timestamp as u64, Bytes::copy_from_slice(data), key)
    }
}
