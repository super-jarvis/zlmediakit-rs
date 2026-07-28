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
            self.buffer[9],
            self.buffer[10],
            self.buffer[11],
            self.buffer[12],
        ]);

        self.buffer.advance(header_size as usize);
        Some(flags & 0x01 != 0)
    }

    pub fn parse_tag(&mut self) -> Option<MediaFrame> {
        if self.buffer.len() < 15 {
            return None;
        }

        let tag_type = self.buffer[0];
        let data_size = ((self.buffer[1] as u32) << 16)
            | ((self.buffer[2] as u32) << 8)
            | (self.buffer[3] as u32);
        let timestamp = ((self.buffer[7] as u32) << 16)
            | ((self.buffer[8] as u32) << 8)
            | (self.buffer[9] as u32);

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

                MediaFrame::new_video(0, codec, timestamp, timestamp as u64, timestamp as u64, data, is_key)
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

                MediaFrame::new_audio(1, codec, timestamp, timestamp as u64, timestamp as u64, data)
            }
            _ => {
                return self.parse_tag();
            }
        };

        Some(frame)
    }
}

impl Default for FlvDemuxer {
    fn default() -> Self {
        Self::new()
    }
}
