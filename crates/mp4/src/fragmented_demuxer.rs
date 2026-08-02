//! Incremental ISO BMFF/CMAF fragment demuxing for HLS fMP4 pull inputs.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use std::collections::HashMap;
use zlmediakit_core::media_frame::{
    AudioInfo, CodecId, MediaFrame, PayloadFormat, TrackInfo, VideoInfo,
};

#[derive(Debug, Clone)]
struct FragmentTrack {
    id: u32,
    codec: CodecId,
    timescale: u32,
    width: u32,
    height: u32,
    channels: u32,
    config: Bytes,
}

#[derive(Debug, Clone, Copy)]
struct BoxView {
    kind: [u8; 4],
    start: usize,
    data_start: usize,
    end: usize,
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("truncated ISO BMFF u32"))?;
    Ok(u32::from_be_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow!("truncated ISO BMFF u64"))?;
    Ok(u64::from_be_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn boxes(data: &[u8], start: usize, end: usize) -> Result<Vec<BoxView>> {
    let mut result = Vec::new();
    let mut offset = start;
    while offset + 8 <= end {
        let size32 = read_u32(data, offset)? as usize;
        let kind: [u8; 4] = data[offset + 4..offset + 8]
            .try_into()
            .expect("length checked");
        let (header, size) = match size32 {
            0 => (8, end - offset),
            1 => (16, usize::try_from(read_u64(data, offset + 8)?)?),
            value => (8, value),
        };
        if size < header || offset + size > end {
            bail!("invalid ISO BMFF box size");
        }
        result.push(BoxView {
            kind,
            start: offset,
            data_start: offset + header,
            end: offset + size,
        });
        offset += size;
    }
    Ok(result)
}

fn child(data: &[u8], parent: BoxView, kind: &[u8; 4]) -> Result<Option<BoxView>> {
    Ok(boxes(data, parent.data_start, parent.end)?
        .into_iter()
        .find(|item| &item.kind == kind))
}

fn descendant(data: &[u8], parent: BoxView, path: &[[u8; 4]]) -> Result<Option<BoxView>> {
    let mut current = parent;
    for kind in path {
        let Some(next) = child(data, current, kind)? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

fn descriptor_length(data: &[u8], offset: &mut usize) -> Option<usize> {
    let mut length = 0usize;
    for _ in 0..4 {
        let byte = *data.get(*offset)?;
        *offset += 1;
        length = (length << 7) | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some(length);
        }
    }
    None
}

fn extract_asc(esds: &[u8]) -> Bytes {
    let mut offset = 4.min(esds.len());
    while offset < esds.len() {
        let tag = esds[offset];
        offset += 1;
        let Some(length) = descriptor_length(esds, &mut offset) else {
            break;
        };
        if offset + length > esds.len() {
            break;
        }
        if tag == 0x05 {
            return Bytes::copy_from_slice(&esds[offset..offset + length]);
        }
        // Descriptor 0x03/0x04 nest their child descriptors after fixed
        // headers; a byte scan is more tolerant of optional flags.
        if let Some(relative) = esds[offset..offset + length]
            .iter()
            .position(|byte| *byte == 0x05)
        {
            let mut child_offset = offset + relative + 1;
            if let Some(child_length) = descriptor_length(esds, &mut child_offset) {
                if child_offset + child_length <= offset + length {
                    return Bytes::copy_from_slice(
                        &esds[child_offset..child_offset + child_length],
                    );
                }
            }
        }
        offset += length;
    }
    Bytes::new()
}

/// Demuxes an HLS CMAF init segment plus subsequent `moof`/`mdat` fragments.
pub struct Fmp4Demuxer {
    tracks: HashMap<u32, FragmentTrack>,
}

impl Fmp4Demuxer {
    pub fn from_init_segment(data: &[u8]) -> Result<Self> {
        let top = boxes(data, 0, data.len())?;
        let moov = top
            .into_iter()
            .find(|item| &item.kind == b"moov")
            .ok_or_else(|| anyhow!("CMAF init segment is missing moov"))?;
        let mut tracks = HashMap::new();
        for trak in boxes(data, moov.data_start, moov.end)?
            .into_iter()
            .filter(|item| &item.kind == b"trak")
        {
            let tkhd = child(data, trak, b"tkhd")?
                .ok_or_else(|| anyhow!("fragmented MP4 track is missing tkhd"))?;
            let tkhd_data = &data[tkhd.data_start..tkhd.end];
            let version = tkhd_data.first().copied().unwrap_or(0);
            let id_offset = if version == 1 { 20 } else { 12 };
            let id = read_u32(tkhd_data, id_offset)?;
            let width = tkhd_data
                .len()
                .checked_sub(8)
                .map(|offset| read_u32(tkhd_data, offset).unwrap_or(0) >> 16)
                .unwrap_or(0);
            let height = tkhd_data
                .len()
                .checked_sub(4)
                .map(|offset| read_u32(tkhd_data, offset).unwrap_or(0) >> 16)
                .unwrap_or(0);

            let mdia = child(data, trak, b"mdia")?
                .ok_or_else(|| anyhow!("fragmented MP4 track is missing mdia"))?;
            let mdhd = child(data, mdia, b"mdhd")?
                .ok_or_else(|| anyhow!("fragmented MP4 track is missing mdhd"))?;
            let mdhd_data = &data[mdhd.data_start..mdhd.end];
            let timescale_offset = if mdhd_data.first().copied().unwrap_or(0) == 1 {
                20
            } else {
                12
            };
            let timescale = read_u32(mdhd_data, timescale_offset)?.max(1);
            let stsd = descendant(data, mdia, &[*b"minf", *b"stbl", *b"stsd"])?
                .ok_or_else(|| anyhow!("fragmented MP4 track is missing stsd"))?;
            let stsd_data = &data[stsd.data_start..stsd.end];
            if stsd_data.len() < 16 {
                bail!("fragmented MP4 stsd is truncated");
            }
            let entry_size = read_u32(stsd_data, 8)? as usize;
            if entry_size < 8 || 8 + entry_size > stsd_data.len() {
                bail!("invalid fragmented MP4 sample entry");
            }
            let entry_kind = &stsd_data[12..16];
            let (codec, child_offset, channels) = match entry_kind {
                b"avc1" | b"avc3" => (CodecId::H264, 8 + 86, 0),
                b"hvc1" | b"hev1" => (CodecId::H265, 8 + 86, 0),
                b"mp4a" => {
                    let channels = stsd_data
                        .get(8 + 24..8 + 26)
                        .map(|bytes| u16::from_be_bytes(bytes.try_into().unwrap()) as u32)
                        .unwrap_or(2);
                    (CodecId::AAC, 8 + 36, channels)
                }
                other => bail!(
                    "unsupported fragmented MP4 sample entry {}",
                    String::from_utf8_lossy(other)
                ),
            };
            let entry_start = stsd.data_start + 8;
            let entry_end = entry_start + entry_size;
            let config_kind = match codec {
                CodecId::H264 => b"avcC",
                CodecId::H265 => b"hvcC",
                _ => b"esds",
            };
            let config_box = boxes(data, stsd.data_start + child_offset, entry_end)?
                .into_iter()
                .find(|item| &item.kind == config_kind)
                .ok_or_else(|| anyhow!("fragmented MP4 sample entry is missing codec config"))?;
            let raw_config = &data[config_box.data_start..config_box.end];
            let config = if codec == CodecId::AAC {
                extract_asc(raw_config)
            } else {
                Bytes::copy_from_slice(raw_config)
            };
            tracks.insert(
                id,
                FragmentTrack {
                    id,
                    codec,
                    timescale,
                    width,
                    height,
                    channels,
                    config,
                },
            );
        }
        if tracks.is_empty() {
            bail!("CMAF init segment contains no supported tracks");
        }
        Ok(Self { tracks })
    }

    pub fn track_info(&self) -> Vec<TrackInfo> {
        self.tracks
            .values()
            .map(|track| {
                if track.codec.is_video() {
                    TrackInfo::Video(VideoInfo {
                        codec: track.codec,
                        width: track.width,
                        height: track.height,
                        fps: 0.0,
                        key_frame: true,
                    })
                } else {
                    TrackInfo::Audio(AudioInfo {
                        codec: track.codec,
                        sample_rate: track.timescale,
                        channels: track.channels.max(1),
                        bits_per_sample: 16,
                    })
                }
            })
            .collect()
    }

    pub fn config_frames(&self) -> Vec<MediaFrame> {
        self.tracks
            .values()
            .filter(|track| !track.config.is_empty())
            .map(|track| {
                let mut frame = if track.codec.is_video() {
                    MediaFrame::new_video(
                        track.id,
                        track.codec,
                        0,
                        0,
                        0,
                        track.config.clone(),
                        false,
                    )
                    .with_payload_format(if track.codec == CodecId::H264 {
                        PayloadFormat::Avcc
                    } else {
                        PayloadFormat::Hvcc
                    })
                } else {
                    MediaFrame::new_audio(track.id, track.codec, 0, 0, 0, track.config.clone())
                        .with_payload_format(PayloadFormat::AacAudioSpecificConfig)
                };
                frame.config_frame = true;
                frame
            })
            .collect()
    }

    pub fn demux_fragment(&self, data: &[u8]) -> Result<Vec<MediaFrame>> {
        let top = boxes(data, 0, data.len())?;
        let moof = top
            .iter()
            .copied()
            .find(|item| &item.kind == b"moof")
            .ok_or_else(|| anyhow!("CMAF media segment is missing moof"))?;
        let mdat = top
            .iter()
            .copied()
            .find(|item| &item.kind == b"mdat")
            .ok_or_else(|| anyhow!("CMAF media segment is missing mdat"))?;
        let mut frames = Vec::new();
        for traf in boxes(data, moof.data_start, moof.end)?
            .into_iter()
            .filter(|item| &item.kind == b"traf")
        {
            self.demux_traf(data, moof, mdat, traf, &mut frames)?;
        }
        frames.sort_by_key(|frame| (frame.dts, frame.track_id));
        Ok(frames)
    }

    fn demux_traf(
        &self,
        data: &[u8],
        moof: BoxView,
        mdat: BoxView,
        traf: BoxView,
        output: &mut Vec<MediaFrame>,
    ) -> Result<()> {
        let tfhd =
            child(data, traf, b"tfhd")?.ok_or_else(|| anyhow!("CMAF traf is missing tfhd"))?;
        let tfhd_data = &data[tfhd.data_start..tfhd.end];
        let flags = read_u32(tfhd_data, 0)? & 0x00ff_ffff;
        let track_id = read_u32(tfhd_data, 4)?;
        let track = self
            .tracks
            .get(&track_id)
            .ok_or_else(|| anyhow!("CMAF fragment references unknown track {track_id}"))?;
        let mut tfhd_offset = 8;
        let base_data_offset = if flags & 0x000001 != 0 {
            let value = usize::try_from(read_u64(tfhd_data, tfhd_offset)?)?;
            tfhd_offset += 8;
            value
        } else {
            moof.start
        };
        if flags & 0x000002 != 0 {
            tfhd_offset += 4;
        }
        let default_duration = if flags & 0x000008 != 0 {
            let value = read_u32(tfhd_data, tfhd_offset)?;
            tfhd_offset += 4;
            value
        } else {
            0
        };
        let default_size = if flags & 0x000010 != 0 {
            let value = read_u32(tfhd_data, tfhd_offset)?;
            tfhd_offset += 4;
            value
        } else {
            0
        };
        let default_flags = if flags & 0x000020 != 0 {
            read_u32(tfhd_data, tfhd_offset)?
        } else {
            0
        };
        let base_decode_time = if let Some(tfdt) = child(data, traf, b"tfdt")? {
            let tfdt_data = &data[tfdt.data_start..tfdt.end];
            if tfdt_data.first().copied().unwrap_or(0) == 1 {
                read_u64(tfdt_data, 4)?
            } else {
                u64::from(read_u32(tfdt_data, 4)?)
            }
        } else {
            0
        };
        let mut decode_time = base_decode_time;
        let mut implicit_data_offset = mdat.data_start;
        for trun in boxes(data, traf.data_start, traf.end)?
            .into_iter()
            .filter(|item| &item.kind == b"trun")
        {
            let trun_data = &data[trun.data_start..trun.end];
            let version = trun_data.first().copied().unwrap_or(0);
            let trun_flags = read_u32(trun_data, 0)? & 0x00ff_ffff;
            let sample_count = read_u32(trun_data, 4)? as usize;
            let mut offset = 8;
            let mut sample_offset = if trun_flags & 0x000001 != 0 {
                let relative = read_u32(trun_data, offset)? as i32;
                offset += 4;
                usize::try_from((base_data_offset as i64) + i64::from(relative))
                    .context("negative CMAF trun data offset")?
            } else {
                implicit_data_offset
            };
            let first_sample_flags = if trun_flags & 0x000004 != 0 {
                let value = Some(read_u32(trun_data, offset)?);
                offset += 4;
                value
            } else {
                None
            };
            for index in 0..sample_count {
                let duration = if trun_flags & 0x000100 != 0 {
                    let value = read_u32(trun_data, offset)?;
                    offset += 4;
                    value
                } else {
                    default_duration
                };
                let size = if trun_flags & 0x000200 != 0 {
                    let value = read_u32(trun_data, offset)?;
                    offset += 4;
                    value
                } else {
                    default_size
                } as usize;
                let sample_flags = if trun_flags & 0x000400 != 0 {
                    let value = read_u32(trun_data, offset)?;
                    offset += 4;
                    value
                } else if index == 0 {
                    first_sample_flags.unwrap_or(default_flags)
                } else {
                    default_flags
                };
                let composition_offset = if trun_flags & 0x000800 != 0 {
                    let raw = read_u32(trun_data, offset)?;
                    offset += 4;
                    if version == 1 {
                        i64::from(raw as i32)
                    } else {
                        i64::from(raw)
                    }
                } else {
                    0
                };
                let sample = data
                    .get(sample_offset..sample_offset + size)
                    .ok_or_else(|| anyhow!("CMAF sample data is truncated"))?;
                let dts_ms = decode_time.saturating_mul(1000) / u64::from(track.timescale);
                let pts_ticks = if composition_offset.is_negative() {
                    decode_time.saturating_sub(composition_offset.unsigned_abs())
                } else {
                    decode_time.saturating_add(composition_offset as u64)
                };
                let pts_ms = pts_ticks.saturating_mul(1000) / u64::from(track.timescale);
                let key = sample_flags & 0x0001_0000 == 0;
                let frame = if track.codec.is_video() {
                    MediaFrame::new_video(
                        track.id,
                        track.codec,
                        dts_ms as u32,
                        pts_ms,
                        dts_ms,
                        Bytes::copy_from_slice(sample),
                        key,
                    )
                    .with_payload_format(if track.codec == CodecId::H264 {
                        PayloadFormat::Avcc
                    } else {
                        PayloadFormat::Hvcc
                    })
                } else {
                    MediaFrame::new_audio(
                        track.id,
                        track.codec,
                        dts_ms as u32,
                        pts_ms,
                        dts_ms,
                        Bytes::copy_from_slice(sample),
                    )
                    .with_payload_format(PayloadFormat::AacRaw)
                };
                output.push(frame);
                decode_time = decode_time.saturating_add(u64::from(duration));
                sample_offset += size;
            }
            implicit_data_offset = sample_offset;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fmp4Muxer;

    #[test]
    fn demuxes_muxer_init_and_media_fragment() {
        let mut muxer = Fmp4Muxer::new(90_000);
        let mut config = MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::from_static(&[
                0x17, 0, 0, 0, 0, 0x01, 0x64, 0, 0x0a, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x0a, 1, 0,
                4, 0x68, 0xee, 0x3c, 0x80,
            ]),
            false,
        )
        .with_payload_format(PayloadFormat::Flv);
        config.config_frame = true;
        muxer.push_frame(&config);
        muxer.push_frame(
            &MediaFrame::new_video(
                0,
                CodecId::H264,
                0,
                0,
                0,
                Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
                true,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        );

        let demuxer = Fmp4Demuxer::from_init_segment(&muxer.init_segment().data).unwrap();
        assert_eq!(demuxer.track_info().len(), 1);
        assert_eq!(demuxer.config_frames()[0].codec, CodecId::H264);
        let segment = muxer.flush_segment().unwrap();
        let frames = demuxer.demux_fragment(&segment.data).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].codec, CodecId::H264);
        assert!(frames[0].key_frame);
        assert_eq!(frames[0].payload_format, PayloadFormat::Avcc);
        assert_eq!(&frames[0].data[..], &[0, 0, 0, 4, 0x65, 1, 2, 3]);
    }

    #[test]
    fn demuxes_aac_track_and_raw_sample() {
        let mut muxer = Fmp4Muxer::new(90_000);
        let mut config = MediaFrame::new_audio(
            1,
            CodecId::AAC,
            0,
            0,
            0,
            Bytes::from_static(&[0xaf, 0, 0x12, 0x10]),
        )
        .with_payload_format(PayloadFormat::Flv);
        config.config_frame = true;
        muxer.push_frame(&config);
        muxer.push_frame(
            &MediaFrame::new_audio(
                1,
                CodecId::AAC,
                0,
                0,
                0,
                Bytes::from_static(&[0xaf, 1, 0x11, 0x22, 0x33]),
            )
            .with_payload_format(PayloadFormat::Flv),
        );

        let demuxer = Fmp4Demuxer::from_init_segment(&muxer.init_segment().data).unwrap();
        assert!(demuxer
            .track_info()
            .iter()
            .any(|track| matches!(track, TrackInfo::Audio(audio) if audio.codec == CodecId::AAC)));
        assert_eq!(demuxer.config_frames()[0].data.as_ref(), &[0x12, 0x10]);
        let frames = demuxer
            .demux_fragment(&muxer.flush_segment().unwrap().data)
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].codec, CodecId::AAC);
        assert_eq!(frames[0].payload_format, PayloadFormat::AacRaw);
        assert_eq!(frames[0].data.as_ref(), &[0x11, 0x22, 0x33]);
    }
}
