use crate::media_frame::{CodecId, FrameType, MediaFrame, TrackInfo};
use crate::payload::{
    aac_audio_specific_config, aac_raw_payload, video_config_annex_b, video_sample_annex_b,
};
use anyhow::{bail, Result};

const VIDEO_STREAM_ID: u8 = 0xe0;
const AUDIO_STREAM_ID: u8 = 0xc0;
const MAX_PES_PAYLOAD: usize = 65_512;
const PTS_MASK: u64 = (1u64 << 33) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PsTrack {
    stream_type: u8,
    stream_id: u8,
}

#[derive(Debug, Clone, Copy)]
struct AacConfig {
    profile: u8,
    sample_rate_index: u8,
    channel_config: u8,
}

/// Stateful MPEG-2 Program Stream muxer for GB28181 PS-over-RTP output.
///
/// Timestamps on [`MediaFrame`] are milliseconds and are converted to the
/// MPEG 90 kHz clock. The muxer emits a pack header for every access unit and
/// refreshes the system header/program stream map when the stream description
/// changes or when a video key frame arrives.
#[derive(Debug)]
pub struct PsMuxer {
    tracks: Vec<PsTrack>,
    aac_config: Option<AacConfig>,
    map_pending: bool,
}

impl PsMuxer {
    pub fn new(tracks: &[TrackInfo]) -> Self {
        let mut muxer = Self {
            tracks: Vec::new(),
            aac_config: None,
            map_pending: true,
        };
        for track in tracks {
            match track {
                TrackInfo::Video(video) => muxer.add_codec(video.codec),
                TrackInfo::Audio(audio) => {
                    muxer.add_codec(audio.codec);
                    if audio.codec == CodecId::AAC {
                        muxer.aac_config = aac_config_from_track(audio.sample_rate, audio.channels);
                    }
                }
            }
        }
        muxer
    }

    pub fn mux_frame(&mut self, frame: &MediaFrame) -> Result<Vec<u8>> {
        if frame.frame_type == FrameType::Metadata {
            return Ok(Vec::new());
        }
        let track = ps_track(frame.codec)?;
        if !self.tracks.contains(&track) {
            self.tracks.push(track);
            self.map_pending = true;
        }

        if frame.codec == CodecId::AAC && frame.config_frame {
            let asc = aac_audio_specific_config(frame)?;
            self.aac_config = parse_aac_config(&asc);
            return Ok(Vec::new());
        }

        let payload = self.elementary_payload(frame)?;
        if payload.is_empty() {
            return Ok(Vec::new());
        }

        let scr = timestamp_90k(frame.dts);
        let mut output = pack_header(scr);
        if self.map_pending || (frame.frame_type == FrameType::Video && frame.key_frame) {
            output.extend_from_slice(&system_header(&self.tracks));
            output.extend_from_slice(&program_stream_map(&self.tracks));
            self.map_pending = false;
        }

        let pts = timestamp_90k(frame.pts);
        let dts = timestamp_90k(frame.dts);
        for chunk in payload.chunks(MAX_PES_PAYLOAD) {
            output.extend_from_slice(&pes_packet(track.stream_id, pts, dts, chunk));
        }
        Ok(output)
    }

    fn add_codec(&mut self, codec: CodecId) {
        if let Ok(track) = ps_track(codec) {
            if !self.tracks.contains(&track) {
                self.tracks.push(track);
            }
        }
    }

    fn elementary_payload(&self, frame: &MediaFrame) -> Result<Vec<u8>> {
        match frame.codec {
            CodecId::H264 | CodecId::H265 => Ok(if frame.config_frame {
                video_config_annex_b(frame)?.to_vec()
            } else {
                video_sample_annex_b(frame)?.to_vec()
            }),
            CodecId::AAC => {
                let raw = aac_raw_payload(frame)?;
                let config = self.aac_config.ok_or_else(|| {
                    anyhow::anyhow!("AAC configuration is required for PS output")
                })?;
                let mut output = adts_header(config, raw.len())?;
                output.extend_from_slice(&raw);
                Ok(output)
            }
            CodecId::G711A | CodecId::G711U | CodecId::Mp3 | CodecId::MP2A => {
                Ok(raw_audio_payload(frame))
            }
            codec => bail!("PS muxing codec {codec:?} is not supported"),
        }
    }
}

fn ps_track(codec: CodecId) -> Result<PsTrack> {
    let (stream_type, stream_id) = match codec {
        CodecId::H264 => (0x1b, VIDEO_STREAM_ID),
        CodecId::H265 => (0x24, VIDEO_STREAM_ID),
        CodecId::AAC => (0x0f, AUDIO_STREAM_ID),
        CodecId::G711A => (0x90, AUDIO_STREAM_ID),
        CodecId::G711U => (0x91, AUDIO_STREAM_ID),
        CodecId::MP2A => (0x04, AUDIO_STREAM_ID),
        CodecId::Mp3 => (0x03, AUDIO_STREAM_ID),
        codec => bail!("PS muxing codec {codec:?} is not supported"),
    };
    Ok(PsTrack {
        stream_type,
        stream_id,
    })
}

fn timestamp_90k(milliseconds: u64) -> u64 {
    milliseconds.wrapping_mul(90) & PTS_MASK
}

fn pack_header(scr: u64) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.push(0b01, 2);
    bits.push(scr >> 30, 3);
    bits.push(1, 1);
    bits.push(scr >> 15, 15);
    bits.push(1, 1);
    bits.push(scr, 15);
    bits.push(1, 1);
    bits.push(0, 9);
    bits.push(1, 1);
    bits.push(50_000, 22);
    bits.push(1, 1);
    bits.push(1, 1);
    bits.push(0x1f, 5);
    bits.push(0, 3);
    let mut output = vec![0, 0, 1, 0xba];
    output.extend_from_slice(&bits.finish());
    output
}

fn system_header(tracks: &[PsTrack]) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.push(1, 1);
    bits.push(50_000, 22);
    bits.push(1, 1);
    bits.push(
        tracks
            .iter()
            .filter(|track| track.stream_id & 0xe0 == 0xc0)
            .count()
            .min(0x3f) as u64,
        6,
    );
    bits.push(0, 1);
    bits.push(0, 1);
    bits.push(0, 1);
    bits.push(0, 1);
    bits.push(1, 1);
    bits.push(
        tracks
            .iter()
            .filter(|track| track.stream_id & 0xf0 == 0xe0)
            .count()
            .min(0x1f) as u64,
        5,
    );
    bits.push(0, 1);
    bits.push(0x7f, 7);
    for track in tracks {
        bits.push(track.stream_id as u64, 8);
        bits.push(0b11, 2);
        let video = track.stream_id & 0xf0 == 0xe0;
        bits.push(u64::from(video), 1);
        bits.push(if video { 512 } else { 32 }, 13);
    }
    let body = bits.finish();
    let mut output = vec![0, 0, 1, 0xbb];
    output.extend_from_slice(&(body.len() as u16).to_be_bytes());
    output.extend_from_slice(&body);
    output
}

fn program_stream_map(tracks: &[PsTrack]) -> Vec<u8> {
    let mut elementary_map = Vec::with_capacity(tracks.len() * 4);
    for track in tracks {
        elementary_map.push(track.stream_type);
        elementary_map.push(track.stream_id);
        elementary_map.extend_from_slice(&0u16.to_be_bytes());
    }
    let map_length = 10 + elementary_map.len();
    let mut output = vec![0, 0, 1, 0xbc];
    output.extend_from_slice(&(map_length as u16).to_be_bytes());
    output.extend_from_slice(&[0xe0, 0xff]);
    output.extend_from_slice(&0u16.to_be_bytes());
    output.extend_from_slice(&(elementary_map.len() as u16).to_be_bytes());
    output.extend_from_slice(&elementary_map);
    let crc = mpeg2_crc32(&output);
    output.extend_from_slice(&crc.to_be_bytes());
    output
}

fn pes_packet(stream_id: u8, pts: u64, dts: u64, payload: &[u8]) -> Vec<u8> {
    let include_dts = pts != dts;
    let header_data_length = if include_dts { 10 } else { 5 };
    let packet_length = 3 + header_data_length + payload.len();
    let mut output = vec![0, 0, 1, stream_id];
    output.extend_from_slice(&(packet_length as u16).to_be_bytes());
    output.push(0x80);
    output.push(if include_dts { 0xc0 } else { 0x80 });
    output.push(header_data_length as u8);
    output.extend_from_slice(&encode_timestamp(if include_dts { 0x3 } else { 0x2 }, pts));
    if include_dts {
        output.extend_from_slice(&encode_timestamp(0x1, dts));
    }
    output.extend_from_slice(payload);
    output
}

fn encode_timestamp(prefix: u8, value: u64) -> [u8; 5] {
    let value = value & PTS_MASK;
    [
        (prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1,
        (value >> 22) as u8,
        (((value >> 15) as u8 & 0x7f) << 1) | 1,
        (value >> 7) as u8,
        ((value as u8 & 0x7f) << 1) | 1,
    ]
}

fn parse_aac_config(data: &[u8]) -> Option<AacConfig> {
    if data.len() < 2 {
        return None;
    }
    let object_type = data[0] >> 3;
    let sample_rate_index = ((data[0] & 0x07) << 1) | (data[1] >> 7);
    let channel_config = (data[1] >> 3) & 0x0f;
    (object_type > 0 && sample_rate_index < 13).then_some(AacConfig {
        profile: object_type.saturating_sub(1).min(3),
        sample_rate_index,
        channel_config,
    })
}

fn aac_config_from_track(sample_rate: u32, channels: u32) -> Option<AacConfig> {
    const SAMPLE_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let sample_rate_index = SAMPLE_RATES.iter().position(|rate| *rate == sample_rate)? as u8;
    Some(AacConfig {
        profile: 1,
        sample_rate_index,
        channel_config: channels.min(7) as u8,
    })
}

fn adts_header(config: AacConfig, payload_length: usize) -> Result<Vec<u8>> {
    let frame_length = payload_length + 7;
    if frame_length > 0x1fff {
        bail!("AAC access unit is too large for an ADTS frame");
    }
    let channels = config.channel_config;
    Ok(vec![
        0xff,
        0xf1,
        (config.profile << 6) | (config.sample_rate_index << 2) | (channels >> 2),
        ((channels & 0x03) << 6) | ((frame_length >> 11) as u8 & 0x03),
        (frame_length >> 3) as u8,
        (((frame_length & 0x07) as u8) << 5) | 0x1f,
        0xfc,
    ])
}

fn raw_audio_payload(frame: &MediaFrame) -> Vec<u8> {
    let flv = frame.payload_format == crate::media_frame::PayloadFormat::Flv
        || (frame.payload_format == crate::media_frame::PayloadFormat::Unknown
            && frame
                .data
                .first()
                .is_some_and(|header| matches!(header >> 4, 2 | 7 | 8)));
    if flv && !frame.data.is_empty() {
        frame.data[1..].to_vec()
    } else {
        frame.data.to_vec()
    }
}

fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= (*byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
}

impl BitWriter {
    fn push(&mut self, value: u64, bit_count: u8) {
        for shift in (0..bit_count).rev() {
            self.current = (self.current << 1) | ((value >> shift) as u8 & 1);
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.current);
                self.current = 0;
                self.used = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used != 0 {
            self.current <<= 8 - self.used;
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_frame::{PayloadFormat, VideoInfo};
    use bytes::Bytes;

    fn h264_frame(timestamp: u32) -> MediaFrame {
        MediaFrame::new_video(
            0,
            CodecId::H264,
            timestamp,
            timestamp as u64,
            timestamp as u64,
            Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
            true,
        )
        .with_payload_format(PayloadFormat::AnnexB)
    }

    #[test]
    fn muxes_pack_system_map_and_video_pes() {
        let mut muxer = PsMuxer::new(&[TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1920,
            height: 1080,
            fps: 25.0,
            key_frame: true,
        })]);
        let ps = muxer.mux_frame(&h264_frame(1_000)).unwrap();
        assert_eq!(&ps[..4], &[0, 0, 1, 0xba]);
        assert!(ps.windows(4).any(|window| window == [0, 0, 1, 0xbb]));
        assert!(ps.windows(4).any(|window| window == [0, 0, 1, 0xbc]));
        let pes = ps
            .windows(4)
            .position(|window| window == [0, 0, 1, VIDEO_STREAM_ID])
            .unwrap();
        assert_eq!(ps[pes + 7], 0x80);
        assert_eq!(decode_timestamp(&ps[pes + 9..pes + 14]), 90_000);
        assert!(ps.windows(4).any(|window| window == [0, 0, 0, 1]));
    }

    #[test]
    fn program_stream_map_crc_covers_the_complete_map() {
        let map = program_stream_map(&[
            ps_track(CodecId::H264).unwrap(),
            ps_track(CodecId::AAC).unwrap(),
        ]);
        let expected = u32::from_be_bytes(map[map.len() - 4..].try_into().unwrap());
        assert_eq!(mpeg2_crc32(&map[..map.len() - 4]), expected);
    }

    #[test]
    fn aac_payload_gets_an_adts_header() {
        let mut muxer = PsMuxer::new(&[TrackInfo::Audio(crate::media_frame::AudioInfo {
            codec: CodecId::AAC,
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
        })]);
        let frame = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            20,
            20,
            20,
            Bytes::from_static(&[1, 2, 3, 4]),
        )
        .with_payload_format(PayloadFormat::AacRaw);
        let ps = muxer.mux_frame(&frame).unwrap();
        assert!(ps.windows(2).any(|window| window == [0xff, 0xf1]));
    }

    fn decode_timestamp(data: &[u8]) -> u64 {
        (((data[0] >> 1) as u64 & 0x07) << 30)
            | ((data[1] as u64) << 22)
            | (((data[2] >> 1) as u64 & 0x7f) << 15)
            | ((data[3] as u64) << 7)
            | ((data[4] >> 1) as u64 & 0x7f)
    }
}
