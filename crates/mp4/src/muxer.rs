use bytes::{BufMut, Bytes, BytesMut};
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};
use zlmediakit_core::{
    aac_audio_specific_config, aac_raw_payload, video_decoder_config, video_sample_length_prefixed,
};

pub struct Mp4Muxer {
    video_codec: Option<CodecId>,
    audio_codec: Option<CodecId>,
    width: u32,
    height: u32,
    fps: f64,
    sample_rate: u32,
    channels: u32,
    video_config: Option<Bytes>,
    audio_config: Option<Bytes>,
    samples: Vec<SampleEntry>,
    base_time: Option<u32>,
}

struct SampleEntry {
    track_id: u32,
    data: Bytes,
    timestamp: u32,
    duration: u32,
    #[allow(dead_code)]
    is_sync: bool,
}

impl Mp4Muxer {
    pub fn new() -> Self {
        Self {
            video_codec: None,
            audio_codec: None,
            width: 0,
            height: 0,
            fps: 30.0,
            sample_rate: 0,
            channels: 0,
            video_config: None,
            audio_config: None,
            samples: Vec::new(),
            base_time: None,
        }
    }

    pub fn push_frame(&mut self, frame: &MediaFrame) {
        if self.base_time.is_none() {
            self.base_time = Some(frame.timestamp);
        }

        if frame.config_frame || zlmediakit_core::gop_cache::is_config_frame(frame) {
            match frame.frame_type {
                FrameType::Video => {
                    let config = match video_decoder_config(frame) {
                        Ok(config) => config,
                        Err(_) => return,
                    };
                    self.video_codec = Some(frame.codec);
                    self.video_config = Some(config.clone());
                    if let Some(info) = extract_video_info(&frame.codec, &config) {
                        self.width = info.0;
                        self.height = info.1;
                        self.fps = info.2;
                    }
                }
                FrameType::Audio => {
                    let config = if frame.codec == CodecId::AAC {
                        match aac_audio_specific_config(frame) {
                            Ok(config) => config,
                            Err(_) => return,
                        }
                    } else {
                        frame.data.clone()
                    };
                    self.audio_codec = Some(frame.codec);
                    self.audio_config = Some(config.clone());
                    self.sample_rate = config_to_sample_rate(&config);
                    self.channels = 1;
                }
                _ => {}
            }
            return;
        }

        let (is_video, codec) = match frame.frame_type {
            FrameType::Video => (true, frame.codec),
            FrameType::Audio => (false, frame.codec),
            _ => return,
        };

        if is_video {
            if self.video_codec.is_none() {
                self.video_codec = Some(codec);
            }
        } else {
            if self.audio_codec.is_none() {
                self.audio_codec = Some(codec);
            }
        }
        if frame.frame_type == FrameType::Audio && self.audio_config.is_none() {
            self.audio_config = Some(Bytes::new());
        }

        let base = self.base_time.unwrap_or(0);
        let ts = frame.timestamp.saturating_sub(base);

        let data = match frame.frame_type {
            FrameType::Video => match video_sample_length_prefixed(frame) {
                Ok(data) => data,
                Err(_) => return,
            },
            FrameType::Audio if frame.codec == CodecId::AAC => match aac_raw_payload(frame) {
                Ok(data) => data,
                Err(_) => return,
            },
            _ => frame.data.clone(),
        };

        let sample = SampleEntry {
            track_id: if is_video { 0 } else { 1 },
            data,
            timestamp: ts,
            duration: 0,
            is_sync: is_video && frame.key_frame,
        };
        self.samples.push(sample);
    }

    pub fn finalize(&mut self) -> BytesMut {
        if self.samples.is_empty() {
            return BytesMut::new();
        }

        self.compute_durations();
        let video_samples: Vec<&SampleEntry> =
            self.samples.iter().filter(|s| s.track_id == 0).collect();
        let audio_samples: Vec<&SampleEntry> =
            self.samples.iter().filter(|s| s.track_id == 1).collect();

        let has_video = !video_samples.is_empty();
        let has_audio = !audio_samples.is_empty();

        let video_timescale: u32 = 90000;
        let audio_timescale = if self.sample_rate > 0 {
            self.sample_rate
        } else {
            44100
        };

        let video_duration_media: u64 = video_samples.iter().map(|s| s.duration as u64).sum();
        let audio_duration_media: u64 = audio_samples.iter().map(|s| s.duration as u64).sum();
        let video_duration_ms = if has_video {
            video_duration_media * 1000 / video_timescale as u64
        } else {
            0
        };
        let audio_duration_ms = if has_audio {
            audio_duration_media * 1000 / audio_timescale as u64
        } else {
            0
        };
        let duration_ms = video_duration_ms.max(audio_duration_ms);

        let mut ftyp = BytesMut::new();
        write_ftyp(&mut ftyp);

        let mut mdat_data = BytesMut::new();
        let mut video_mdat_offset: u32 = 0;
        let mut audio_mdat_offset: u32 = 0;

        if has_video {
            video_mdat_offset = mdat_data.len() as u32;
            for s in &video_samples {
                mdat_data.extend_from_slice(&s.data);
            }
        }
        if has_audio {
            audio_mdat_offset = mdat_data.len() as u32;
            for s in &audio_samples {
                mdat_data.extend_from_slice(&s.data);
            }
        }

        let video_sample_sizes: Vec<u32> =
            video_samples.iter().map(|s| s.data.len() as u32).collect();
        let audio_sample_sizes: Vec<u32> =
            audio_samples.iter().map(|s| s.data.len() as u32).collect();

        let mut moov = BytesMut::new();
        write_mvhd(
            &mut moov,
            duration_ms as u32,
            if has_video && has_audio { 3 } else { 2 },
        );

        if has_video {
            let codec = self.video_codec.unwrap_or(CodecId::H264);
            let config = self.video_config.clone().unwrap_or_default();
            write_trak(
                &mut moov,
                1,
                b"vide",
                &codec,
                &config,
                self.width,
                self.height,
                video_timescale,
                video_duration_media as u32,
                &video_sample_sizes,
                &video_samples,
                video_mdat_offset,
                ftyp.len() as u32,
            );
        }

        if has_audio {
            let codec = self.audio_codec.unwrap_or(CodecId::AAC);
            let config = self.audio_config.clone().unwrap_or_default();
            let ts = audio_timescale;
            write_trak(
                &mut moov,
                if has_video { 2 } else { 1 },
                b"soun",
                &codec,
                &config,
                0,
                0,
                ts,
                audio_duration_media as u32,
                &audio_sample_sizes,
                &audio_samples,
                audio_mdat_offset,
                ftyp.len() as u32,
            );
        }

        let moov_box = make_box(b"moov", &moov);
        let mdat_box = make_box(b"mdat", &mdat_data);

        let mut output = BytesMut::new();
        output.extend_from_slice(&ftyp);
        output.extend_from_slice(&mdat_box);
        output.extend_from_slice(&moov_box);
        output
    }

    fn compute_durations(&mut self) {
        if self.samples.is_empty() {
            return;
        }
        let fps = if self.fps > 0.0 { self.fps } else { 30.0 };

        let mut last_ts_v: Option<u32> = None;
        let mut last_ts_a: Option<u32> = None;

        for s in &mut self.samples {
            if s.track_id == 0 {
                if let Some(prev) = last_ts_v {
                    let d = s.timestamp.saturating_sub(prev).max(1);
                    let d_scaled = (d as u64 * 90_000 / 1000) as u32;
                    s.duration = d_scaled.max(1);
                } else {
                    let frame_dur = (90_000.0 / fps) as u32;
                    s.duration = frame_dur.max(1);
                }
                last_ts_v = Some(s.timestamp);
            } else {
                if let Some(prev) = last_ts_a {
                    let d = s.timestamp.saturating_sub(prev).max(1);
                    let sr = if self.sample_rate > 0 {
                        self.sample_rate
                    } else {
                        44100
                    };
                    let d_scaled = (d as u64 * sr as u64 / 1000) as u32;
                    s.duration = d_scaled.max(1);
                } else {
                    let sr = if self.sample_rate > 0 {
                        self.sample_rate
                    } else {
                        44100
                    };
                    let frame_dur = sr / 44; // ~23ms per AAC frame
                    s.duration = frame_dur.max(1);
                }
                last_ts_a = Some(s.timestamp);
            }
        }

        if last_ts_v.is_some() {
            for s in self.samples.iter_mut().rev() {
                if s.track_id == 0 {
                    s.duration = s.duration.max(1);
                    break;
                }
            }
        }
        if last_ts_a.is_some() {
            for s in self.samples.iter_mut().rev() {
                if s.track_id == 1 {
                    s.duration = s.duration.max(1);
                    break;
                }
            }
        }
    }
}

impl Default for Mp4Muxer {
    fn default() -> Self {
        Self::new()
    }
}

fn make_box(box_type: &[u8; 4], data: &[u8]) -> BytesMut {
    let size = 8 + data.len();
    let mut buf = BytesMut::with_capacity(size);
    buf.put_u32(size as u32);
    buf.extend_from_slice(box_type);
    buf.extend_from_slice(data);
    buf
}

fn write_full_box(buf: &mut BytesMut, box_type: &[u8; 4], version: u8, flags: u32, data: &[u8]) {
    let size = 12 + data.len();
    buf.put_u32(size as u32);
    buf.extend_from_slice(box_type);
    buf.put_u8(version);
    buf.put_u8(((flags >> 16) & 0xFF) as u8);
    buf.put_u8(((flags >> 8) & 0xFF) as u8);
    buf.put_u8((flags & 0xFF) as u8);
    buf.extend_from_slice(data);
}

fn write_ftyp(buf: &mut BytesMut) {
    let data = {
        let mut d = BytesMut::new();
        d.extend_from_slice(b"isom");
        d.put_u32(0x200);
        d.extend_from_slice(b"isom");
        d.extend_from_slice(b"iso2");
        d.extend_from_slice(b"avc1");
        d.extend_from_slice(b"mp41");
        d
    };
    buf.extend_from_slice(&make_box(b"ftyp", &data));
}

fn write_mvhd(buf: &mut BytesMut, duration_ms: u32, next_track_id: u32) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u32(0); // creation_time
        d.put_u32(0); // modification_time
        d.put_u32(1000); // timescale
        d.put_u32(duration_ms); // duration
        d.put_u32(0x00010000); // rate (1.0 fixed-point)
        d.put_u16(0x0100); // volume (1.0)
        d.put_u16(0); // reserved
        d.put_u32(0); // reserved[0]
        d.put_u32(0); // reserved[1]
        let matrix: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
        for value in matrix {
            d.put_u32(value);
        }
        d.put_u32(0); // pre_defined
        d.put_u32(0);
        d.put_u32(0);
        d.put_u32(0);
        d.put_u32(0);
        d.put_u32(0);
        d.put_u32(next_track_id); // next_track_id
        d
    };
    write_full_box(buf, b"mvhd", 0, 0, &data);
}

#[allow(clippy::too_many_arguments)]
fn write_trak(
    buf: &mut BytesMut,
    track_id: u32,
    handler_type: &[u8; 4],
    codec: &CodecId,
    config: &[u8],
    width: u32,
    height: u32,
    timescale: u32,
    duration: u32,
    sample_sizes: &[u32],
    samples: &[&SampleEntry],
    mdat_offset: u32,
    before_mdat: u32,
) {
    let mut trak = BytesMut::new();

    write_tkhd(
        &mut trak,
        track_id,
        (duration as u64 * 1000 / timescale.max(1) as u64) as u32,
        width,
        height,
    );

    let mut minf = BytesMut::new();
    if *handler_type == *b"vide" {
        write_vmhd(&mut minf);
    } else {
        write_smhd(&mut minf);
    }
    write_dinf(&mut minf);
    write_stbl(
        &mut minf,
        codec,
        config,
        width,
        height,
        timescale,
        sample_sizes,
        samples,
        mdat_offset,
        before_mdat,
    );

    let mut mdia = BytesMut::new();
    write_mdhd(&mut mdia, timescale, duration);
    write_hdlr(&mut mdia, handler_type);
    mdia.extend_from_slice(&make_box(b"minf", &minf));

    trak.extend_from_slice(&make_box(b"mdia", &mdia));
    buf.extend_from_slice(&make_box(b"trak", &trak));
}

fn write_tkhd(buf: &mut BytesMut, track_id: u32, duration: u32, width: u32, height: u32) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u32(0); // creation_time
        d.put_u32(0); // modification_time
        d.put_u32(track_id);
        d.put_u32(0); // reserved
        d.put_u32(duration);
        d.put_u32(0); // reserved[0]
        d.put_u32(0); // reserved[1]
        d.put_u16(0); // layer
        d.put_u16(0); // alternate_group
        d.put_u16(if width == 0 { 0x0100 } else { 0 }); // volume
        d.put_u16(0); // reserved
        let m: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
        for &v in &m {
            d.put_u32(v);
        }
        d.put_u32((width as u64 * 65536) as u32);
        d.put_u32((height as u64 * 65536) as u32);
        d
    };
    write_full_box(buf, b"tkhd", 0, 0x0007, &data);
}

fn write_mdhd(buf: &mut BytesMut, timescale: u32, duration: u32) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u32(0); // creation_time
        d.put_u32(0); // modification_time
        d.put_u32(timescale);
        d.put_u32(duration);
        d.put_u16(0x55C4); // language (und)
        d.put_u16(0); // quality
        d
    };
    write_full_box(buf, b"mdhd", 0, 0, &data);
}

fn write_hdlr(buf: &mut BytesMut, handler_type: &[u8; 4]) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u32(0); // pre_defined
        d.extend_from_slice(handler_type);
        d.put_u32(0); // reserved
        d.put_u32(0); // reserved
        d.put_u32(0); // reserved
        d.extend_from_slice(b"zlmediakit-rs");
        d.put_u8(0);
        d
    };
    write_full_box(buf, b"hdlr", 0, 0, &data);
}

fn write_vmhd(buf: &mut BytesMut) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u16(0); // graphics_mode
        d.put_u16(0); // opcolor[0]
        d.put_u16(0); // opcolor[1]
        d.put_u16(0); // opcolor[2]
        d
    };
    write_full_box(buf, b"vmhd", 0, 0x0001, &data);
}

fn write_smhd(buf: &mut BytesMut) {
    let data = {
        let mut d = BytesMut::new();
        d.put_u16(0); // balance
        d.put_u16(0); // reserved
        d
    };
    write_full_box(buf, b"smhd", 0, 0, &data);
}

fn write_dinf(buf: &mut BytesMut) {
    let mut url_box = BytesMut::new();
    write_full_box(&mut url_box, b"url ", 0, 1, &[]);
    let mut dref_data = BytesMut::new();
    dref_data.put_u32(1); // entry_count
    dref_data.extend_from_slice(&url_box);
    let mut dref_box = BytesMut::new();
    write_full_box(&mut dref_box, b"dref", 0, 0, &dref_data);
    buf.extend_from_slice(&make_box(b"dinf", &dref_box));
}

#[allow(clippy::too_many_arguments)]
fn write_stbl(
    buf: &mut BytesMut,
    codec: &CodecId,
    config: &[u8],
    width: u32,
    height: u32,
    timescale: u32,
    sample_sizes: &[u32],
    samples: &[&SampleEntry],
    mdat_offset: u32,
    before_mdat: u32,
) {
    write_stsd(buf, codec, config, width, height, timescale);
    write_stts(buf, samples);
    write_stsc(buf, samples.len());
    write_stsz(buf, sample_sizes);
    write_stco(buf, before_mdat + 8 + mdat_offset);
}

fn write_stsd(
    buf: &mut BytesMut,
    codec: &CodecId,
    config: &[u8],
    width: u32,
    height: u32,
    _timescale: u32,
) {
    let mut stsd_data = BytesMut::new();
    stsd_data.put_u32(0); // version + flags
    stsd_data.put_u32(1); // entry_count

    match codec {
        CodecId::H264 => {
            let mut sample = BytesMut::new();
            sample.put_u32(0); // reserved
            sample.put_u16(0); // reserved
            sample.put_u16(1); // data_reference_index
            sample.put_u16(0); // pre_defined
            sample.put_u16(0); // reserved
            sample.put_u32(0); // pre_defined[0]
            sample.put_u32(0); // pre_defined[1]
            sample.put_u32(0); // pre_defined[2]
            sample.put_u16(width as u16); // width
            sample.put_u16(height as u16); // height
            sample.put_u32(0x00480000); // horizresolution
            sample.put_u32(0x00480000); // vertresolution
            sample.put_u32(0); // reserved
            sample.put_u16(1); // frame_count
            sample.extend_from_slice(&[0u8; 32]); // compressorname
            sample.put_u16(0x0018); // depth
            sample.put_u16(-1i16 as u16); // pre_defined

            let mut avcc = BytesMut::new();
            avcc.extend_from_slice(&parse_avcc_config(config));

            let avcc_box = make_box(b"avcC", &avcc);
            sample.extend_from_slice(&avcc_box);

            stsd_data.extend_from_slice(&make_box(b"avc1", &sample));
        }
        CodecId::H265 => {
            let mut sample = BytesMut::new();
            sample.put_u32(0);
            sample.put_u16(0);
            sample.put_u16(1);
            sample.put_u16(0);
            sample.put_u16(0);
            sample.put_u32(0);
            sample.put_u32(0);
            sample.put_u32(0);
            sample.put_u16(width as u16);
            sample.put_u16(height as u16);
            sample.put_u32(0x00480000);
            sample.put_u32(0x00480000);
            sample.put_u32(0);
            sample.put_u16(1);
            sample.extend_from_slice(&[0u8; 32]);
            sample.put_u16(0x0018);
            sample.put_u16(-1i16 as u16);

            let mut hvcc = BytesMut::new();
            hvcc.extend_from_slice(&parse_hvcc_config(config));

            let hvcc_box = make_box(b"hvcC", &hvcc);
            sample.extend_from_slice(&hvcc_box);

            stsd_data.extend_from_slice(&make_box(b"hev1", &sample));
        }
        CodecId::AAC => {
            let mut sample = BytesMut::new();
            sample.put_u32(0); // reserved
            sample.put_u16(0); // reserved
            sample.put_u16(1); // data_reference_index
            let version = 0u16;
            sample.put_u16(version); // version
            sample.put_u16(0); // revision_level
            sample.put_u32(0); // vendor
            sample.put_u16(2); // channel_count
            sample.put_u16(16); // sample_size
            sample.put_u16(0); // compression_id
            sample.put_u16(0); // packet_size
            sample.put_u32(config_to_sample_rate(config) << 16); // sample_rate (16.16)

            let mut esds = BytesMut::new();
            esds.put_u32(0); // version + flags
            let audio_config = if config.is_empty() {
                vec![0x12, 0x10]
            } else {
                config.to_vec()
            };
            let desc = build_esds_descriptors(2, &audio_config);
            esds.extend_from_slice(&desc);

            let esds_box = make_box(b"esds", &esds);
            sample.extend_from_slice(&esds_box);

            stsd_data.extend_from_slice(&make_box(b"mp4a", &sample));
        }
        _ => {}
    }

    buf.extend_from_slice(&make_box(b"stsd", &stsd_data));
}

fn write_stts(buf: &mut BytesMut, samples: &[&SampleEntry]) {
    if samples.is_empty() {
        let empty = {
            let mut d = BytesMut::new();
            d.put_u32(0);
            d.put_u32(0);
            d
        };
        write_full_box(buf, b"stts", 0, 0, &empty);
        return;
    }

    let mut entries: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        let dur = s.duration.max(1);
        if let Some(last) = entries.last_mut() {
            if last.1 == dur {
                last.0 += 1;
                continue;
            }
        }
        entries.push((1, dur));
    }

    let mut data = BytesMut::new();
    data.put_u32(0); // version + flags
    data.put_u32(entries.len() as u32);
    for (count, dur) in &entries {
        data.put_u32(*count);
        data.put_u32(*dur);
    }
    buf.extend_from_slice(&make_box(b"stts", &data));
}

fn write_stsc(buf: &mut BytesMut, sample_count: usize) {
    let mut data = BytesMut::new();
    data.put_u32(0); // version + flags
    if sample_count == 0 {
        data.put_u32(0);
    } else {
        data.put_u32(1);
        data.put_u32(1);
        data.put_u32(sample_count as u32);
        data.put_u32(1);
    }
    buf.extend_from_slice(&make_box(b"stsc", &data));
}

fn write_stsz(buf: &mut BytesMut, sample_sizes: &[u32]) {
    let same = if sample_sizes.is_empty() {
        0
    } else {
        let first = sample_sizes[0];
        if sample_sizes.iter().all(|s| *s == first) {
            first
        } else {
            0
        }
    };
    let mut data = BytesMut::new();
    data.put_u32(0); // version + flags
    data.put_u32(same); // sample_size
    data.put_u32(sample_sizes.len() as u32);
    if same == 0 {
        for sz in sample_sizes {
            data.put_u32(*sz);
        }
    }
    buf.extend_from_slice(&make_box(b"stsz", &data));
}

fn write_stco(buf: &mut BytesMut, chunk_offset: u32) {
    let mut data = BytesMut::new();
    data.put_u32(0); // version + flags
    data.put_u32(1);
    data.put_u32(chunk_offset);
    buf.extend_from_slice(&make_box(b"stco", &data));
}

fn parse_avcc_config(data: &[u8]) -> Vec<u8> {
    if data.first() == Some(&1) && data.len() >= 7 {
        return data.to_vec();
    }
    // data includes the 5-byte FLV video tag header [codec_id, packet_type, comp_time(3)],
    // so the actual AVCDecoderConfigurationRecord starts at offset 5.
    let inner = if data.len() > 5 {
        &data[5..]
    } else {
        return vec![1, 0x42, 0, 0x1e, 0xff, 0xe1, 0, 25];
    };
    if inner.is_empty() {
        return vec![1, 0x42, 0, 0x1e, 0xff, 0xe1, 0, 25];
    }
    let mut avcc = Vec::with_capacity(inner.len() + 8);
    avcc.push(1);
    avcc.push(inner[0]);
    avcc.push(0);
    avcc.push(inner[1].max(inner[2]));
    avcc.push(0xff);

    let nals = split_nalus_avcc(inner);
    let sps_list: Vec<&[u8]> = nals
        .iter()
        .filter(|n| n[0] & 0x1f == 7)
        .map(|n| n.as_ref())
        .collect();
    let pps_list: Vec<&[u8]> = nals
        .iter()
        .filter(|n| n[0] & 0x1f == 8)
        .map(|n| n.as_ref())
        .collect();

    avcc.push(0xe0 | sps_list.len() as u8);
    for sps in &sps_list {
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
    }
    avcc.push(pps_list.len() as u8);
    for pps in &pps_list {
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
    }
    avcc
}

fn parse_hvcc_config(data: &[u8]) -> Vec<u8> {
    if data.first() == Some(&1) && data.len() >= 23 {
        return data.to_vec();
    }
    let inner = if data.len() > 5 {
        &data[5..]
    } else {
        return vec![1];
    };
    if inner.is_empty() {
        return vec![1];
    }
    let nals = split_nalus_avcc(inner);
    let vps_list: Vec<&[u8]> = nals
        .iter()
        .filter(|n| (n[0] >> 1) == 16)
        .map(|n| n.as_ref())
        .collect();
    let sps_list: Vec<&[u8]> = nals
        .iter()
        .filter(|n| (n[0] >> 1) == 17)
        .map(|n| n.as_ref())
        .collect();
    let pps_list: Vec<&[u8]> = nals
        .iter()
        .filter(|n| (n[0] >> 1) == 18)
        .map(|n| n.as_ref())
        .collect();

    let mut hvcc = Vec::new();
    hvcc.push(1); // configurationVersion
    if let Some(vps) = vps_list.first() {
        hvcc.push(vps[2] & 0xFC); // general_profile_space + tier + profile_idc
        hvcc.push(0); // profile_compatibility
        hvcc.push(vps[3] & 0xE0 | (vps[4] >> 3)); // level_idc
    } else {
        hvcc.extend_from_slice(&[0, 0, 0]);
    }
    hvcc.push(0xFC); // constraint_indicator_flags
    hvcc.push(0);
    hvcc.push(0);
    hvcc.push(0);

    hvcc.push(vps_list.len() as u8 & 0x0F);
    hvcc.push(sps_list.len() as u8 & 0x0F);
    hvcc.push(pps_list.len() as u8 & 0x3F);

    for vps in &vps_list {
        hvcc.extend_from_slice(&(vps.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(vps);
    }
    for sps in &sps_list {
        hvcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(sps);
    }
    for pps in &pps_list {
        hvcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(pps);
    }
    hvcc
}

fn split_nalus_avcc(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if i + len <= data.len() {
            nals.push(data[i..i + len].to_vec());
            i += len;
        } else {
            break;
        }
    }
    nals
}

fn build_esds_descriptors(_channels: u32, audio_specific_config: &[u8]) -> Vec<u8> {
    let mut esds = Vec::new();

    esds.push(0x03);
    let es_id = 1u16;
    let mut desc_data = Vec::new();
    desc_data.extend_from_slice(&es_id.to_be_bytes());
    desc_data.push(0x00);
    desc_data.push(0x00);

    let mut decoder_config = Vec::new();
    decoder_config.push(0x04);
    let dc_data = vec![0x40, 0x15, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00];

    let mut dec_specific = Vec::new();
    dec_specific.push(0x05);
    let asc_len = audio_specific_config.len() as u8;
    dec_specific.push(asc_len);
    dec_specific.extend_from_slice(audio_specific_config);

    let dc_len = dc_data.len() + dec_specific.len();
    put_ber_length(&mut decoder_config, dc_len);
    decoder_config.extend_from_slice(&dc_data);
    decoder_config.extend_from_slice(&dec_specific);

    let sl_config = vec![0x06, 1, 0x02];

    let desc_len = desc_data.len() + decoder_config.len() + sl_config.len();
    put_ber_length(&mut esds, desc_len);
    esds.extend_from_slice(&desc_data);
    esds.extend_from_slice(&decoder_config);
    esds.extend_from_slice(&sl_config);
    esds
}

fn put_ber_length(buf: &mut Vec<u8>, len: usize) {
    if len < 128 {
        buf.push(len as u8);
    } else if len < 16384 {
        buf.push(0x80 | (len >> 7) as u8);
        buf.push((len & 0x7F) as u8);
    } else {
        buf.push(0x80 | (len >> 14) as u8);
        buf.push(0x80 | ((len >> 7) & 0x7F) as u8);
        buf.push((len & 0x7F) as u8);
    }
}

fn config_to_sample_rate(config: &[u8]) -> u32 {
    if config.len() >= 2 {
        let idx = ((config[0] as u32 & 0x07) << 1) | ((config[1] as u32 >> 7) & 1);
        match idx {
            0 => 96000,
            1 => 88200,
            2 => 64000,
            3 => 48000,
            4 => 44100,
            5 => 32000,
            6 => 24000,
            7 => 22050,
            8 => 16000,
            9 => 12000,
            10 => 11025,
            11 => 8000,
            12 => 7350,
            _ => 44100,
        }
    } else {
        44100
    }
}

fn extract_video_info(codec: &CodecId, data: &[u8]) -> Option<(u32, u32, f64)> {
    let inner = if data.len() > 5 { &data[5..] } else { data };
    match codec {
        CodecId::H264 => {
            let nals = split_nalus_avcc(inner);
            for nal in &nals {
                if nal.len() >= 5 && (nal[0] & 0x1f) == 7 {
                    let width = ((nal[3] as u32) << 8) | (nal[4] as u32);
                    let height = if nal.len() > 6 {
                        ((nal[5] as u32) << 8) | (nal[6] as u32)
                    } else {
                        0
                    };
                    return Some((width, height, 30.0));
                }
            }
            None
        }
        CodecId::H265 => {
            let nals = split_nalus_avcc(inner);
            for nal in &nals {
                if nal.len() >= 6 && (nal[0] >> 1) == 17 {
                    let width = ((nal[4] as u32) << 8) | (nal[5] as u32);
                    let height = if nal.len() > 7 {
                        ((nal[6] as u32) << 8) | (nal[7] as u32)
                    } else {
                        0
                    };
                    return Some((width, height, 30.0));
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use zlmediakit_core::media_frame::{CodecId, MediaFrame};

    fn h264_config() -> MediaFrame {
        // FLV AVC sequence header (packet type 0) with SPS and PPS
        let data = vec![
            0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x05, 0x67,
            0x42, 0x00, 0x1e, 0xa6, 0x01, 0x68, 0xce, 0x3c, 0x80,
        ];
        MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, Bytes::from(data), true)
    }

    fn h264_key_frame(pts: u32, payload: &[u8]) -> MediaFrame {
        let mut data = vec![0x17, 0x01, 0x00, 0x00, 0x00];
        data.extend_from_slice(payload);
        MediaFrame::new_video(
            0,
            CodecId::H264,
            pts,
            pts as u64,
            pts as u64,
            Bytes::from(data),
            true,
        )
    }

    fn h264_inter_frame(pts: u32, payload: &[u8]) -> MediaFrame {
        let mut data = vec![0x27, 0x01, 0x00, 0x00, 0x00];
        data.extend_from_slice(payload);
        MediaFrame::new_video(
            0,
            CodecId::H264,
            pts,
            pts as u64,
            pts as u64,
            Bytes::from(data),
            false,
        )
    }

    fn aac_config() -> MediaFrame {
        let data = vec![0xAF, 0x00, 0x12, 0x10]; // AAC sequence header
        MediaFrame::new_audio(1, CodecId::AAC, 0, 0, 0, Bytes::from(data))
    }

    fn aac_frame(pts: u32) -> MediaFrame {
        MediaFrame::new_audio(
            1,
            CodecId::AAC,
            pts,
            pts as u64,
            pts as u64,
            Bytes::from(vec![0x12, 0x10, 0x01, 0x02]),
        )
    }

    #[test]
    fn new_muxer_empty() {
        let mut muxer = Mp4Muxer::new();
        let out = muxer.finalize();
        assert!(out.is_empty());
    }

    #[test]
    fn finalize_with_video_only() {
        let mut muxer = Mp4Muxer::new();
        muxer.push_frame(&h264_config());
        muxer.push_frame(&h264_key_frame(0, &[0x01, 0x02, 0x03]));
        muxer.push_frame(&h264_inter_frame(33, &[0x04, 0x05]));
        muxer.push_frame(&h264_inter_frame(66, &[0x06, 0x07]));
        let out = muxer.finalize();
        assert!(!out.is_empty());
        // Should start with ftyp box
        assert_eq!(&out[4..8], b"ftyp");
        // Should contain moov box
        assert!(out.windows(4).any(|w| w == b"moov"));
        // Should contain mdat box
        assert!(out.windows(4).any(|w| w == b"mdat"));
        // Should contain avc1 and avcC
        assert!(out.windows(4).any(|w| w == b"avc1"));
        assert!(out.windows(4).any(|w| w == b"avcC"));
    }

    #[test]
    fn finalize_with_audio_only() {
        let mut muxer = Mp4Muxer::new();
        muxer.push_frame(&aac_config());
        muxer.push_frame(&aac_frame(0));
        muxer.push_frame(&aac_frame(23));
        let out = muxer.finalize();
        assert!(!out.is_empty());
        assert!(out.windows(4).any(|w| w == b"mp4a"));
        assert!(out.windows(4).any(|w| w == b"esds"));
    }

    #[test]
    fn finalize_with_video_and_audio() {
        let mut muxer = Mp4Muxer::new();
        muxer.push_frame(&h264_config());
        muxer.push_frame(&aac_config());
        muxer.push_frame(&h264_key_frame(0, &[0x01]));
        muxer.push_frame(&aac_frame(10));
        muxer.push_frame(&h264_inter_frame(33, &[0x02]));
        muxer.push_frame(&aac_frame(40));
        let out = muxer.finalize();
        assert!(!out.is_empty());
        assert!(out.windows(4).any(|w| w == b"moov"));
        assert!(out.windows(4).any(|w| w == b"mdat"));
        assert!(out.windows(4).any(|w| w == b"mp4a"));
    }

    #[test]
    fn compute_durations_handles_empty() {
        let mut muxer = Mp4Muxer::new();
        muxer.compute_durations();
        // Should not panic
    }

    #[test]
    fn make_box_creates_correct_format() {
        let data = b"\x00\x00\x00\x01";
        let box_data = make_box(b"test", data);
        assert_eq!(box_data.len(), 12);
        assert_eq!(&box_data[0..4], &12u32.to_be_bytes());
        assert_eq!(&box_data[4..8], b"test");
        assert_eq!(&box_data[8..], data);
    }

    #[test]
    fn write_ftyp_produces_valid_box() {
        let mut buf = BytesMut::new();
        write_ftyp(&mut buf);
        assert_eq!(&buf[4..8], b"ftyp");
        assert_eq!(&buf[8..12], b"isom");
    }

    #[test]
    fn write_stsd_for_audio() {
        let mut buf = BytesMut::new();
        let codec = CodecId::AAC;
        let config = b"\x12\x10";
        write_stsd(&mut buf, &codec, config, 0, 0, 44100);
        assert!(buf.windows(4).any(|w| w == b"mp4a"));
        assert!(buf.windows(4).any(|w| w == b"esds"));
    }

    #[test]
    fn push_frame_order_preserved() {
        let mut muxer = Mp4Muxer::new();
        let mut cfg = h264_config();
        cfg.config_frame = true;
        muxer.push_frame(&cfg);
        muxer.push_frame(&h264_key_frame(0, &[0xAA]));
        muxer.push_frame(&h264_inter_frame(33, &[0xBB]));
        muxer.push_frame(&h264_key_frame(66, &[0xCC]));
        muxer.finalize();
        assert_eq!(muxer.samples.len(), 3);
        assert_eq!(&muxer.samples[0].data[..], b"\xAA");
    }

    #[test]
    fn push_frame_strips_flv_headers_from_samples() {
        use zlmediakit_core::PayloadFormat;

        let mut muxer = Mp4Muxer::new();
        let frame = MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::from_static(&[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88]),
            true,
        )
        .with_payload_format(PayloadFormat::Flv);
        muxer.push_frame(&frame);

        assert_eq!(muxer.samples.len(), 1);
        assert_eq!(muxer.samples[0].data.as_ref(), &[0, 0, 0, 2, 0x65, 0x88]);
    }
}
