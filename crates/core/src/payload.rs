//! Conversion helpers for the byte representation carried by `MediaFrame`.
//!
//! These helpers deliberately operate on complete media frames rather than on
//! protocol sessions. Protocol crates can therefore share one validated set of
//! conversions instead of each making a different assumption about `data`.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{Error, Result};
use crate::media_frame::{CodecId, FrameType, MediaFrame, PayloadFormat};

const FLV_VIDEO_HEADER_LEN: usize = 5;
const FLV_AUDIO_HEADER_LEN: usize = 2;

/// Returns an H.264/H.265 decoder configuration record without an FLV header.
pub fn video_decoder_config(frame: &MediaFrame) -> Result<Bytes> {
    if !matches!(frame.codec, CodecId::H264 | CodecId::H265) {
        return Err(Error::Unsupported(format!(
            "video decoder config for {:?}",
            frame.codec
        )));
    }
    let format = declared_or_detected_video_format(frame);
    let flv_config =
        format == PayloadFormat::Flv && frame.data.len() > 1 && frame.data[1] & 0x0f == 0;
    if !frame.config_frame && !flv_config {
        return Err(Error::Codec("expected a video configuration frame".into()));
    }

    match format {
        PayloadFormat::Flv => {
            let data = frame.data.as_ref();
            if data.len() <= FLV_VIDEO_HEADER_LEN || data[1] & 0x0f != 0 {
                return Err(Error::Codec(
                    "invalid FLV video configuration payload".into(),
                ));
            }
            Ok(frame.data.slice(FLV_VIDEO_HEADER_LEN..))
        }
        PayloadFormat::Avcc if frame.codec == CodecId::H264 => Ok(frame.data.clone()),
        PayloadFormat::Hvcc if frame.codec == CodecId::H265 => Ok(frame.data.clone()),
        PayloadFormat::Unknown => Ok(frame.data.clone()),
        other => Err(Error::Unsupported(format!(
            "cannot derive {:?} decoder config from {:?}",
            frame.codec, other
        ))),
    }
}

/// Returns an MP4-compatible length-prefixed H.264/H.265 access unit.
pub fn video_sample_length_prefixed(frame: &MediaFrame) -> Result<Bytes> {
    if frame.config_frame {
        return Err(Error::Codec("expected a video media sample".into()));
    }
    if !matches!(frame.codec, CodecId::H264 | CodecId::H265) {
        return Err(Error::Unsupported(format!(
            "length-prefixed sample for {:?}",
            frame.codec
        )));
    }

    match declared_or_detected_video_format(frame) {
        PayloadFormat::Flv => {
            let data = frame.data.as_ref();
            if data.len() <= FLV_VIDEO_HEADER_LEN || data[1] & 0x0f != 1 {
                return Err(Error::Codec("invalid FLV video media payload".into()));
            }
            Ok(frame.data.slice(FLV_VIDEO_HEADER_LEN..))
        }
        PayloadFormat::Avcc | PayloadFormat::Hvcc => Ok(frame.data.clone()),
        PayloadFormat::AnnexB => annex_b_to_length_prefixed(&frame.data),
        PayloadFormat::Unknown => Ok(frame.data.clone()),
        other => Err(Error::Unsupported(format!(
            "cannot convert {:?} video payload to a length-prefixed sample",
            other
        ))),
    }
}

/// Returns an Annex-B H.264/H.265 access unit.
pub fn video_sample_annex_b(frame: &MediaFrame) -> Result<Bytes> {
    if frame.config_frame {
        return Err(Error::Codec("expected a video media sample".into()));
    }

    match declared_or_detected_video_format(frame) {
        PayloadFormat::AnnexB => Ok(frame.data.clone()),
        PayloadFormat::Flv => {
            let sample = video_sample_length_prefixed(frame)?;
            length_prefixed_to_annex_b(&sample)
        }
        PayloadFormat::Avcc | PayloadFormat::Hvcc | PayloadFormat::Unknown => {
            length_prefixed_to_annex_b(&frame.data)
        }
        other => Err(Error::Unsupported(format!(
            "cannot convert {:?} video payload to Annex-B",
            other
        ))),
    }
}

/// Returns H.264 SPS/PPS or H.265 VPS/SPS/PPS as Annex-B NAL units.
pub fn video_config_annex_b(frame: &MediaFrame) -> Result<Bytes> {
    let config = video_decoder_config(frame)?;
    let mut out = BytesMut::new();
    match frame.codec {
        CodecId::H264 => {
            if config.len() < 6 {
                return Err(Error::Codec("invalid AVCDecoderConfigurationRecord".into()));
            }
            let mut pos = 6;
            let num_sps = (config[5] & 0x1f) as usize;
            append_u16_nalus(&config, &mut pos, num_sps, &mut out)?;
            let num_pps = *config
                .get(pos)
                .ok_or_else(|| Error::Codec("missing AVC PPS count".into()))?
                as usize;
            pos += 1;
            append_u16_nalus(&config, &mut pos, num_pps, &mut out)?;
        }
        CodecId::H265 => {
            if config.len() < 23 {
                return Err(Error::Codec(
                    "invalid HEVCDecoderConfigurationRecord".into(),
                ));
            }
            let mut pos = 23;
            let num_arrays = config[22] as usize;
            for _ in 0..num_arrays {
                if pos + 3 > config.len() {
                    return Err(Error::Codec("truncated HEVC NAL array".into()));
                }
                let nal_type = config[pos] & 0x3f;
                let num_nalus = u16::from_be_bytes([config[pos + 1], config[pos + 2]]) as usize;
                pos += 3;
                for _ in 0..num_nalus {
                    if pos + 2 > config.len() {
                        return Err(Error::Codec("truncated HEVC NAL length".into()));
                    }
                    let len = u16::from_be_bytes([config[pos], config[pos + 1]]) as usize;
                    pos += 2;
                    if pos + len > config.len() {
                        return Err(Error::Codec("truncated HEVC NAL unit".into()));
                    }
                    if matches!(nal_type, 32..=34) {
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(&config[pos..pos + len]);
                    }
                    pos += len;
                }
            }
        }
        codec => {
            return Err(Error::Unsupported(format!(
                "Annex-B decoder configuration for {codec:?}"
            )))
        }
    }
    if out.is_empty() {
        return Err(Error::Codec(
            "decoder configuration has no parameter sets".into(),
        ));
    }
    Ok(out.freeze())
}

fn append_u16_nalus(data: &[u8], pos: &mut usize, count: usize, out: &mut BytesMut) -> Result<()> {
    for _ in 0..count {
        if *pos + 2 > data.len() {
            return Err(Error::Codec("truncated AVC NAL length".into()));
        }
        let len = u16::from_be_bytes([data[*pos], data[*pos + 1]]) as usize;
        *pos += 2;
        if *pos + len > data.len() {
            return Err(Error::Codec("truncated AVC NAL unit".into()));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[*pos..*pos + len]);
        *pos += len;
    }
    Ok(())
}

/// Returns AAC AudioSpecificConfig bytes without an FLV header.
pub fn aac_audio_specific_config(frame: &MediaFrame) -> Result<Bytes> {
    let format = declared_or_detected_aac_format(frame);
    let flv_config = format == PayloadFormat::Flv && frame.data.len() > 1 && frame.data[1] == 0;
    if frame.codec != CodecId::AAC || (!frame.config_frame && !flv_config) {
        return Err(Error::Codec("expected an AAC configuration frame".into()));
    }
    match format {
        PayloadFormat::Flv => {
            if frame.data.len() <= FLV_AUDIO_HEADER_LEN || frame.data[1] != 0 {
                return Err(Error::Codec("invalid FLV AAC configuration payload".into()));
            }
            Ok(frame.data.slice(FLV_AUDIO_HEADER_LEN..))
        }
        PayloadFormat::AacAudioSpecificConfig | PayloadFormat::Unknown => Ok(frame.data.clone()),
        PayloadFormat::Adts => audio_specific_config_from_adts(&frame.data),
        other => Err(Error::Unsupported(format!(
            "cannot derive AAC AudioSpecificConfig from {:?}",
            other
        ))),
    }
}

/// Returns a raw AAC access unit without FLV or ADTS headers.
pub fn aac_raw_payload(frame: &MediaFrame) -> Result<Bytes> {
    if frame.codec != CodecId::AAC || frame.config_frame {
        return Err(Error::Codec("expected an AAC media frame".into()));
    }
    match declared_or_detected_aac_format(frame) {
        PayloadFormat::Flv => {
            if frame.data.len() <= FLV_AUDIO_HEADER_LEN || frame.data[1] != 1 {
                return Err(Error::Codec("invalid FLV AAC media payload".into()));
            }
            Ok(frame.data.slice(FLV_AUDIO_HEADER_LEN..))
        }
        PayloadFormat::AacRaw | PayloadFormat::Unknown => Ok(frame.data.clone()),
        PayloadFormat::Adts => strip_adts_header(&frame.data),
        other => Err(Error::Unsupported(format!(
            "cannot derive raw AAC payload from {:?}",
            other
        ))),
    }
}

/// Returns a complete FLV/RTMP audio or video message payload.
///
/// The returned bytes exclude the outer FLV tag header and RTMP chunk header.
/// H.264/H.265 samples are length-prefixed and AAC samples are raw (without
/// ADTS), as required by their FLV packet layouts.
pub fn flv_payload(frame: &MediaFrame) -> Result<Bytes> {
    if frame.frame_type == FrameType::Metadata || frame.payload_format == PayloadFormat::Flv {
        return Ok(frame.data.clone());
    }

    match frame.frame_type {
        FrameType::Video => {
            let codec_id = match frame.codec {
                CodecId::H264 => 7,
                CodecId::H265 => 12,
                codec => {
                    return Err(Error::Unsupported(format!(
                        "FLV video payload for {codec:?}"
                    )))
                }
            };
            let body = if frame.config_frame {
                video_decoder_config(frame)?
            } else {
                video_sample_length_prefixed(frame)?
            };
            let mut out = Vec::with_capacity(5 + body.len());
            let frame_type = if frame.key_frame || frame.config_frame {
                1
            } else {
                2
            };
            out.push((frame_type << 4) | codec_id);
            out.push(if frame.config_frame { 0 } else { 1 });
            let composition_time = if frame.config_frame {
                0
            } else {
                (frame.pts as i64 - frame.dts as i64).clamp(-8_388_608, 8_388_607) as i32
            };
            out.push(((composition_time >> 16) & 0xff) as u8);
            out.push(((composition_time >> 8) & 0xff) as u8);
            out.push((composition_time & 0xff) as u8);
            out.extend_from_slice(&body);
            Ok(Bytes::from(out))
        }
        FrameType::Audio => match frame.codec {
            CodecId::AAC => {
                let body = if frame.config_frame {
                    aac_audio_specific_config(frame)?
                } else {
                    aac_raw_payload(frame)?
                };
                let mut out = Vec::with_capacity(2 + body.len());
                // AAC, 44 kHz signalling, 16-bit stereo. For AAC, decoders use
                // AudioSpecificConfig for the authoritative rate/channels.
                out.push(0xaf);
                out.push(if frame.config_frame { 0 } else { 1 });
                out.extend_from_slice(&body);
                Ok(Bytes::from(out))
            }
            CodecId::G711A | CodecId::G711U => {
                let mut out = Vec::with_capacity(1 + frame.data.len());
                let sound_format = if frame.codec == CodecId::G711A { 7 } else { 8 };
                out.push((sound_format << 4) | 0x02);
                out.extend_from_slice(&frame.data);
                Ok(Bytes::from(out))
            }
            CodecId::Mp3 => {
                let mut out = Vec::with_capacity(1 + frame.data.len());
                // MP3, 44 kHz signalling, 16-bit stereo. The MPEG audio
                // frame header remains authoritative for the actual format.
                out.push(0x2f);
                out.extend_from_slice(&frame.data);
                Ok(Bytes::from(out))
            }
            codec => Err(Error::Unsupported(format!(
                "FLV audio payload for {codec:?}"
            ))),
        },
        FrameType::Metadata => Ok(frame.data.clone()),
    }
}

fn declared_or_detected_video_format(frame: &MediaFrame) -> PayloadFormat {
    if frame.payload_format != PayloadFormat::Unknown {
        return frame.payload_format;
    }
    let data = frame.data.as_ref();
    if starts_with_annex_b(data) {
        PayloadFormat::AnnexB
    } else if data.len() >= FLV_VIDEO_HEADER_LEN
        && matches!(data[0] & 0x0f, 7 | 12)
        && data[1] & 0x0f <= 1
    {
        PayloadFormat::Flv
    } else if frame.codec == CodecId::H265 {
        PayloadFormat::Hvcc
    } else {
        PayloadFormat::Avcc
    }
}

fn declared_or_detected_aac_format(frame: &MediaFrame) -> PayloadFormat {
    if frame.payload_format != PayloadFormat::Unknown {
        return frame.payload_format;
    }
    let data = frame.data.as_ref();
    if data.len() >= 2 && data[0] >> 4 == 10 && data[1] <= 1 {
        PayloadFormat::Flv
    } else if is_adts(data) {
        PayloadFormat::Adts
    } else if frame.config_frame {
        PayloadFormat::AacAudioSpecificConfig
    } else {
        PayloadFormat::AacRaw
    }
}

fn annex_b_to_length_prefixed(data: &[u8]) -> Result<Bytes> {
    let ranges = annex_b_nalu_ranges(data);
    if ranges.is_empty() {
        return Err(Error::Codec("Annex-B payload contains no NAL units".into()));
    }
    let total = ranges.iter().map(|(start, end)| 4 + end - start).sum();
    let mut out = BytesMut::with_capacity(total);
    for (start, end) in ranges {
        let len = end - start;
        out.put_u32(len as u32);
        out.extend_from_slice(&data[start..end]);
    }
    Ok(out.freeze())
}

fn length_prefixed_to_annex_b(data: &[u8]) -> Result<Bytes> {
    let mut pos = 0;
    let mut out = BytesMut::with_capacity(data.len() + 16);
    while pos + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if len == 0 || pos + len > data.len() {
            return Err(Error::Codec("invalid length-prefixed NAL unit".into()));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos != data.len() || out.is_empty() {
        return Err(Error::Codec("invalid length-prefixed video payload".into()));
    }
    Ok(out.freeze())
}

fn annex_b_nalu_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let start_len = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, i + start_len));
        i += start_len;
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, start))| {
            let end = starts
                .get(idx + 1)
                .map(|(marker_start, _)| *marker_start)
                .unwrap_or(data.len());
            (*start < end).then_some((*start, end))
        })
        .collect()
}

fn starts_with_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn is_adts(data: &[u8]) -> bool {
    data.len() >= 7 && data[0] == 0xff && data[1] & 0xf6 == 0xf0
}

fn adts_header_len(data: &[u8]) -> Result<usize> {
    if !is_adts(data) {
        return Err(Error::Codec("invalid ADTS payload".into()));
    }
    Ok(if data[1] & 0x01 != 0 { 7 } else { 9 })
}

fn strip_adts_header(data: &[u8]) -> Result<Bytes> {
    let header_len = adts_header_len(data)?;
    if data.len() <= header_len {
        return Err(Error::Codec("ADTS frame contains no AAC payload".into()));
    }
    Ok(Bytes::copy_from_slice(&data[header_len..]))
}

fn audio_specific_config_from_adts(data: &[u8]) -> Result<Bytes> {
    adts_header_len(data)?;
    let profile = ((data[2] >> 6) & 0x03) + 1;
    let frequency_index = (data[2] >> 2) & 0x0f;
    let channels = ((data[2] & 0x01) << 2) | ((data[3] >> 6) & 0x03);
    let asc0 = (profile << 3) | (frequency_index >> 1);
    let asc1 = (frequency_index << 7) | (channels << 3);
    Ok(Bytes::from(vec![asc0, asc1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_frame::MediaFrame;

    fn video(data: &[u8], format: PayloadFormat) -> MediaFrame {
        MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::copy_from_slice(data),
            true,
        )
        .with_payload_format(format)
    }

    #[test]
    fn flv_video_sample_is_stripped_for_mp4() {
        let frame = video(
            &[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88],
            PayloadFormat::Flv,
        );
        assert_eq!(
            video_sample_length_prefixed(&frame).unwrap().as_ref(),
            &[0, 0, 0, 2, 0x65, 0x88]
        );
    }

    #[test]
    fn annex_b_round_trips_through_length_prefixes() {
        let input = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2, 3];
        let canonical = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 2, 3];
        let frame = video(&input, PayloadFormat::AnnexB);
        let sample = video_sample_length_prefixed(&frame).unwrap();
        let sample_frame = video(&sample, PayloadFormat::Avcc);
        assert_eq!(
            video_sample_annex_b(&sample_frame).unwrap().as_ref(),
            canonical
        );
    }

    #[test]
    fn flv_and_adts_aac_are_normalized() {
        let flv = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            0,
            0,
            0,
            Bytes::from_static(&[0xaf, 1, 0x11, 0x22]),
        )
        .with_payload_format(PayloadFormat::Flv);
        assert_eq!(aac_raw_payload(&flv).unwrap().as_ref(), &[0x11, 0x22]);

        let adts = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            0,
            0,
            0,
            Bytes::from_static(&[0xff, 0xf1, 0x50, 0x80, 0, 0, 0, 0x11, 0x22]),
        )
        .with_payload_format(PayloadFormat::Adts);
        assert_eq!(aac_raw_payload(&adts).unwrap().as_ref(), &[0x11, 0x22]);
    }

    #[test]
    fn malformed_payloads_return_errors() {
        let frame = video(&[0x17, 1, 0, 0, 0, 0, 0, 1, 0xff], PayloadFormat::Flv);
        assert!(video_sample_annex_b(&frame).is_err());
    }

    #[test]
    fn annex_b_video_is_wrapped_for_flv() {
        let frame = video(&[0, 0, 0, 1, 0x65, 0x88], PayloadFormat::AnnexB);
        assert_eq!(
            flv_payload(&frame).unwrap().as_ref(),
            &[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88]
        );
    }

    #[test]
    fn adts_aac_is_wrapped_without_adts_header_for_flv() {
        let frame = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            23,
            23,
            23,
            Bytes::from_static(&[0xff, 0xf1, 0x50, 0x80, 0, 0, 0, 0x11, 0x22]),
        )
        .with_payload_format(PayloadFormat::Adts);
        assert_eq!(
            flv_payload(&frame).unwrap().as_ref(),
            &[0xaf, 1, 0x11, 0x22]
        );
    }

    #[test]
    fn mp3_is_wrapped_and_opus_is_explicitly_rejected_for_classic_flv() {
        let mp3 = MediaFrame::new_audio(
            1,
            CodecId::Mp3,
            0,
            0,
            0,
            Bytes::from_static(&[0xff, 0xfb, 0x90, 0x64]),
        )
        .with_payload_format(PayloadFormat::Raw);
        assert_eq!(
            flv_payload(&mp3).unwrap().as_ref(),
            &[0x2f, 0xff, 0xfb, 0x90, 0x64]
        );

        let opus = MediaFrame::new_audio(
            1,
            CodecId::Opus,
            0,
            0,
            0,
            Bytes::from_static(&[0xf8, 0xff, 0xfe]),
        )
        .with_payload_format(PayloadFormat::Opus);
        assert!(matches!(flv_payload(&opus), Err(Error::Unsupported(_))));
    }

    #[test]
    fn avcc_config_is_converted_to_annex_b_parameter_sets() {
        let mut frame = video(
            &[
                1, 0x42, 0xc0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0xce,
            ],
            PayloadFormat::Avcc,
        );
        frame.config_frame = true;
        assert_eq!(
            video_config_annex_b(&frame).unwrap().as_ref(),
            &[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce]
        );
    }
}
