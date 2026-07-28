use bytes::{BufMut, BytesMut};
use zlmediakit_core::media_frame::{FrameType, MediaFrame};

pub struct FlvMuxer {
    header_written: bool,
}

impl FlvMuxer {
    pub fn new() -> Self {
        Self {
            header_written: false,
        }
    }

    pub fn write_header(&mut self) -> BytesMut {
        self.header_written = true;
        let mut buf = BytesMut::with_capacity(13);
        buf.extend_from_slice(b"FLV");
        buf.put_u8(0x01);
        buf.put_u8(0x05);
        buf.put_u32(9);
        buf.put_u32(0);
        buf
    }

    pub fn write_tag(&self, frame: &MediaFrame) -> BytesMut {
        let tag_type = match frame.frame_type {
            FrameType::Video => 0x09,
            FrameType::Audio => 0x08,
            FrameType::Metadata => 0x12,
        };

        if frame.frame_type == FrameType::Metadata {
            return self.write_metadata_tag(frame);
        }

        let data_size = frame.data.len() as u32;
        let timestamp = frame.timestamp & 0x7FFFFFFF;

        let mut buf = BytesMut::with_capacity(11 + frame.data.len() + 4);

        buf.put_u8(tag_type);
        buf.put_u8(((data_size >> 16) & 0xFF) as u8);
        buf.put_u8(((data_size >> 8) & 0xFF) as u8);
        buf.put_u8((data_size & 0xFF) as u8);
        buf.put_u8(((timestamp >> 16) & 0xFF) as u8);
        buf.put_u8(((timestamp >> 8) & 0xFF) as u8);
        buf.put_u8((timestamp & 0xFF) as u8);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0);

        buf.extend_from_slice(&frame.data);

        buf.put_u32(11 + data_size);

        buf
    }

    fn write_metadata_tag(&self, frame: &MediaFrame) -> BytesMut {
        let data_size = frame.data.len() as u32;
        let timestamp = frame.timestamp & 0x7FFFFFFF;

        let mut buf = BytesMut::with_capacity(11 + frame.data.len() + 4);

        buf.put_u8(0x12);
        buf.put_u8(((data_size >> 16) & 0xFF) as u8);
        buf.put_u8(((data_size >> 8) & 0xFF) as u8);
        buf.put_u8((data_size & 0xFF) as u8);
        buf.put_u8(((timestamp >> 16) & 0xFF) as u8);
        buf.put_u8(((timestamp >> 8) & 0xFF) as u8);
        buf.put_u8((timestamp & 0xFF) as u8);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0);

        buf.extend_from_slice(&frame.data);

        buf.put_u32(11 + data_size);

        buf
    }

    pub fn write_metadata(
        &self,
        width: u32,
        height: u32,
        fps: f64,
        sample_rate: u32,
    ) -> BytesMut {
        let properties = vec![
            ("duration", "0".to_string()),
            ("width", width.to_string()),
            ("height", height.to_string()),
            ("framerate", fps.to_string()),
            ("videocodecid", "7".to_string()),
            ("audiocodecid", "10".to_string()),
            ("audiosamplerate", sample_rate.to_string()),
            ("audiosamplesize", "16".to_string()),
            ("stereo", "true".to_string()),
            ("encoder", "zlmediakit-rs".to_string()),
        ];

        let mut amf = BytesMut::new();
        amf.put_u8(0x02);
        amf.put_u16(10);
        amf.extend_from_slice(b"onMetaData");

        amf.put_u8(0x08);
        amf.put_u32(properties.len() as u32);

        for (key, value) in &properties {
            amf.put_u16(key.len() as u16);
            amf.extend_from_slice(key.as_bytes());
            if let Ok(n) = value.parse::<f64>() {
                amf.put_u8(0x00);
                amf.put_f64(n);
            } else {
                amf.put_u8(0x02);
                amf.put_u16(value.len() as u16);
                amf.extend_from_slice(value.as_bytes());
            }
        }

        amf.put_u8(0x00);
        amf.put_u8(0x00);
        amf.put_u8(0x09);

        let amf_data = amf.freeze();
        let data_size = amf_data.len() as u32;

        let mut tag = BytesMut::with_capacity(11 + amf_data.len() + 4);
        tag.put_u8(0x12);
        tag.put_u8(((data_size >> 16) & 0xFF) as u8);
        tag.put_u8(((data_size >> 8) & 0xFF) as u8);
        tag.put_u8((data_size & 0xFF) as u8);
        tag.put_u8(0);
        tag.put_u8(0);
        tag.put_u8(0);
        tag.put_u8(0);
        tag.put_u8(0);
        tag.put_u8(0);
        tag.put_u8(0);

        tag.extend_from_slice(&amf_data);
        tag.put_u32(11 + data_size);

        tag
    }
}

impl Default for FlvMuxer {
    fn default() -> Self {
        Self::new()
    }
}
