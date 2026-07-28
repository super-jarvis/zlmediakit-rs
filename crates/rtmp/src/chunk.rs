use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone)]
pub struct RtmpChunkHeader {
    pub fmt: u8,
    pub csid: u32,
    pub timestamp: u32,
    pub msg_length: u32,
    pub msg_type_id: u8,
    pub msg_stream_id: u32,
    pub timestamp_delta: u32,
}

impl RtmpChunkHeader {
    pub fn new() -> Self {
        Self {
            fmt: 0,
            csid: 0,
            timestamp: 0,
            msg_length: 0,
            msg_type_id: 0,
            msg_stream_id: 0,
            timestamp_delta: 0,
        }
    }
}

impl Default for RtmpChunkHeader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct RtmpChunk {
    pub header: RtmpChunkHeader,
    pub payload: Bytes,
}

pub struct RtmpChunkParser {
    pub chunk_size: usize,
}

impl RtmpChunkParser {
    pub fn new() -> Self {
        Self {
            chunk_size: 128,
        }
    }

    pub fn parse_basic_header(byte: u8) -> (u8, u32) {
        let fmt = (byte >> 6) & 0x03;
        let csid = byte & 0x3F;

        match csid {
            0 => (fmt, 64),
            1 => (fmt, 65),
            _ => (fmt, csid as u32),
        }
    }

    pub fn encode_basic_header(fmt: u8, csid: u32) -> BytesMut {
        let mut buf = BytesMut::new();
        if csid >= 64 + 256 {
            buf.put_u8((fmt << 6) | 1);
            let adjusted = csid - 64 - 256;
            buf.put_u8((adjusted & 0xFF) as u8);
            buf.put_u8(((adjusted >> 8) & 0xFF) as u8);
        } else if csid >= 64 {
            buf.put_u8((fmt << 6) | 0);
            buf.put_u8((csid - 64) as u8);
        } else {
            buf.put_u8((fmt << 6) | (csid as u8));
        }
        buf
    }
}

impl Default for RtmpChunkParser {
    fn default() -> Self {
        Self::new()
    }
}
