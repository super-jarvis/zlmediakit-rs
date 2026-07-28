use bytes::{Buf, BytesMut};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};

pub struct HlsDemuxer {
    buffer: BytesMut,
}

impl HlsDemuxer {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub fn parse_m3u8(&self, data: &[u8]) -> Option<Vec<String>> {
        let content = std::str::from_utf8(data).ok()?;
        let mut segments = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            segments.push(line.to_string());
        }

        Some(segments)
    }

    pub fn feed_ts(&mut self, data: &[u8]) -> Vec<MediaFrame> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();

        while self.buffer.len() >= 188 {
            if self.buffer[0] != 0x47 {
                self.buffer.advance(1);
                continue;
            }

            let pid = ((self.buffer[1] as u16 & 0x1F) << 8) | (self.buffer[2] as u16);
            let payload_start = if self.buffer[1] & 0x40 != 0 {
                (self.buffer[3] & 0x0F) as usize + 4
            } else {
                4
            };

            if payload_start < 188 {
                self.buffer.advance(payload_start);
                let payload_len = 188 - payload_start;
                let payload = self.buffer.split_to(payload_len).freeze();
                if pid == 256 {
                    frames.push(MediaFrame::new_video(
                        0,
                        CodecId::H264,
                        0,
                        0,
                        0,
                        payload,
                        true,
                    ));
                } else if pid == 257 {
                    frames.push(MediaFrame::new_audio(
                        1,
                        CodecId::AAC,
                        0,
                        0,
                        0,
                        payload,
                    ));
                }
            }

            self.buffer.advance(188);
        }

        frames
    }
}

impl Default for HlsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}
