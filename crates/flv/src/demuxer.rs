use bytes::{Buf, Bytes, BytesMut};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};

pub struct FlvDemuxer {
    buffer: BytesMut,
}

impl FlvDemuxer {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn parse_header(&mut self) -> Option<bool> {
        if self.buffer.len() < 13 {
            return None;
        }

        if &self.buffer[..3] != b"FLV" {
            return None;
        }

        let _version = self.buffer[3];
        let flags = self.buffer[4];
        let header_size = u32::from_be_bytes([
            self.buffer[5],
            self.buffer[6],
            self.buffer[7],
            self.buffer[8],
        ]);

        self.buffer.advance(header_size as usize);
        Some(flags & 0x01 != 0)
    }

    pub fn parse_tag(&mut self) -> Option<MediaFrame> {
        if self.buffer.len() < 15 {
            return None;
        }

        let tag_type = self.buffer[0];
        // Skip PreviousTagSize (all zeros) that follows the FLV header.
        if tag_type == 0 && (self.buffer[1] | self.buffer[2] | self.buffer[3]) == 0 {
            self.buffer.advance(4);
            return self.parse_tag();
        }

        let data_size = ((self.buffer[1] as u32) << 16)
            | ((self.buffer[2] as u32) << 8)
            | (self.buffer[3] as u32);
        let timestamp = ((self.buffer[4] as u32) << 16)
            | ((self.buffer[5] as u32) << 8)
            | (self.buffer[6] as u32)
            | ((self.buffer[7] as u32) << 24);

        let total_len = 11 + data_size as usize + 4;
        if self.buffer.len() < total_len {
            return None;
        }

        self.buffer.advance(11);
        let payload = self.buffer.split_to(data_size as usize);
        self.buffer.advance(4);

        let frame = match tag_type {
            0x09 => {
                if payload.is_empty() {
                    return self.parse_tag();
                }

                let byte = payload[0];
                let frame_type = (byte >> 4) & 0x0F;
                let codec_id = byte & 0x0F;

                let codec = match codec_id {
                    7 => CodecId::H264,
                    12 => CodecId::H265,
                    _ => CodecId::H264,
                };

                let is_key = frame_type == 1;
                let data = if payload.len() > 5 {
                    payload.freeze().slice(5..)
                } else {
                    Bytes::new()
                };

                MediaFrame::new_video(
                    0,
                    codec,
                    timestamp,
                    timestamp as u64,
                    timestamp as u64,
                    data,
                    is_key,
                )
            }
            0x08 => {
                if payload.is_empty() {
                    return self.parse_tag();
                }

                let byte = payload[0];
                let sound_format = (byte >> 4) & 0x0F;

                let codec = match sound_format {
                    10 => CodecId::AAC,
                    2 => CodecId::Mp3,
                    _ => CodecId::AAC,
                };

                let data = if payload.len() > 2 {
                    payload.freeze().slice(2..)
                } else {
                    Bytes::new()
                };

                MediaFrame::new_audio(
                    1,
                    codec,
                    timestamp,
                    timestamp as u64,
                    timestamp as u64,
                    data,
                )
            }
            _ => {
                return self.parse_tag();
            }
        };

        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlmediakit_core::media_frame::CodecId;

    fn flv_header(has_video: bool, has_audio: bool) -> Vec<u8> {
        let mut flags = 0u8;
        if has_audio {
            flags |= 0x04;
        }
        if has_video {
            flags |= 0x01;
        }
        let mut hdr = vec![b'F', b'L', b'V', 0x01, flags, 0x00, 0x00, 0x00, 0x09];
        hdr.extend_from_slice(&0u32.to_be_bytes()); // previous tag size 0
        hdr
    }

    fn video_tag(data: &[u8], timestamp: u32, is_key: bool) -> Vec<u8> {
        let frame_type = if is_key { 1 } else { 2 };
        let mut payload = vec![(frame_type << 4) | 7]; // AVC/H264
        payload.extend_from_slice(&0u32.to_be_bytes()[1..]); // 3-byte cts
        payload.extend_from_slice(data);
        build_tag(0x09, &payload, timestamp)
    }

    fn audio_tag(data: &[u8], timestamp: u32) -> Vec<u8> {
        let mut payload = vec![0xAF]; // AAC, 44100, 16-bit, stereo
        payload.extend_from_slice(data);
        build_tag(0x08, &payload, timestamp)
    }

    fn build_tag(tag_type: u8, payload: &[u8], timestamp: u32) -> Vec<u8> {
        let mut tag = Vec::with_capacity(15 + payload.len());
        tag.push(tag_type);
        let size = payload.len() as u32;
        tag.extend_from_slice(&[(size >> 16) as u8, (size >> 8) as u8, size as u8]);
        tag.push(((timestamp >> 16) & 0xFF) as u8);
        tag.push(((timestamp >> 8) & 0xFF) as u8);
        tag.push((timestamp & 0xFF) as u8);
        tag.push(0); // extended timestamp
        tag.push(0); // stream ID (3 bytes)
        tag.push(0);
        tag.push(0);
        tag.extend_from_slice(payload);
        tag.extend_from_slice(&size.to_be_bytes()); // previous tag size
        tag
    }

    #[test]
    fn parse_header_with_video() {
        let mut d = FlvDemuxer::new();
        d.feed(&flv_header(true, false));
        assert_eq!(d.parse_header(), Some(true));
    }

    #[test]
    fn parse_header_with_audio() {
        let mut d = FlvDemuxer::new();
        d.feed(&flv_header(false, true));
        assert_eq!(d.parse_header(), Some(false));
    }

    #[test]
    fn parse_header_not_enough_data() {
        let mut d = FlvDemuxer::new();
        d.feed(b"FLV");
        assert_eq!(d.parse_header(), None);
    }

    #[test]
    fn parse_header_invalid_magic() {
        let mut d = FlvDemuxer::new();
        d.feed(b"NOT");
        // still need 13 bytes to reach the check
        let mut buf = b"NOT".to_vec();
        buf.resize(13, 0);
        d.feed(&buf[3..]);
        assert_eq!(d.parse_header(), None);
    }

    #[test]
    fn parse_video_tag() {
        let mut d = FlvDemuxer::new();
        let mut stream = flv_header(true, false);
        stream.extend_from_slice(&video_tag(b"\x00\x00\x01\x67\x42\x00", 100, true));
        d.feed(&stream);
        d.parse_header();
        let frame = d.parse_tag().unwrap();
        assert_eq!(
            frame.frame_type,
            zlmediakit_core::media_frame::FrameType::Video
        );
        assert_eq!(frame.codec, CodecId::H264);
        assert!(frame.key_frame);
    }

    #[test]
    fn parse_audio_tag() {
        let mut d = FlvDemuxer::new();
        let mut stream = flv_header(false, true);
        stream.extend_from_slice(&audio_tag(b"\x12\x10\x01\x02", 200));
        d.feed(&stream);
        d.parse_header();
        let frame = d.parse_tag().unwrap();
        assert_eq!(
            frame.frame_type,
            zlmediakit_core::media_frame::FrameType::Audio
        );
        assert_eq!(frame.codec, CodecId::AAC);
    }

    #[test]
    fn parse_multiple_tags() {
        let mut d = FlvDemuxer::new();
        let mut stream = flv_header(true, true);
        stream.extend_from_slice(&video_tag(b"\x00\x00\x01\x67", 0, true));
        stream.extend_from_slice(&audio_tag(b"\x12\x10", 30));
        stream.extend_from_slice(&video_tag(b"\x00\x00\x01\x68", 40, false));
        d.feed(&stream);
        d.parse_header();
        let v1 = d.parse_tag().unwrap();
        let a1 = d.parse_tag().unwrap();
        let v2 = d.parse_tag().unwrap();
        assert_eq!(v1.timestamp, 0);
        assert_eq!(a1.timestamp, 30);
        assert_eq!(v2.timestamp, 40);
        assert!(v1.key_frame);
        assert!(!v2.key_frame);
    }

    #[test]
    fn parse_tag_insufficient_data() {
        let mut d = FlvDemuxer::new();
        assert!(d.parse_tag().is_none());
    }

    #[test]
    fn parse_tag_skip_unknown_type() {
        let mut d = FlvDemuxer::new();
        let mut stream = flv_header(true, false);
        // Script tag (type 0x12) should be skipped
        let script = build_tag(0x12, b"some data", 0);
        stream.extend_from_slice(&script);
        stream.extend_from_slice(&video_tag(b"\x00\x00\x01\x67", 100, true));
        d.feed(&stream);
        d.parse_header();
        // Should skip script tag and get video tag
        let frame = d.parse_tag().unwrap();
        assert_eq!(frame.timestamp, 100);
    }

    #[test]
    fn video_tag_hevc() {
        let mut d = FlvDemuxer::new();
        let mut stream = flv_header(true, false);
        let mut payload = vec![(1 << 4) | 12]; // key frame, HEVC (codec_id 12)
        payload.extend_from_slice(&0u32.to_be_bytes()[1..]);
        payload.extend_from_slice(b"\x40\x01\x0c");
        stream.extend_from_slice(&build_tag(0x09, &payload, 50));
        d.feed(&stream);
        d.parse_header();
        let frame = d.parse_tag().unwrap();
        assert_eq!(frame.codec, CodecId::H265);
    }
}

impl Default for FlvDemuxer {
    fn default() -> Self {
        Self::new()
    }
}
