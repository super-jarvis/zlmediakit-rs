use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const MSG_SET_CHUNK_SIZE: u8 = 1;
pub const MSG_ABORT: u8 = 2;
pub const MSG_ACK: u8 = 3;
pub const MSG_USER_CONTROL: u8 = 4;
pub const MSG_WINDOW_ACK_SIZE: u8 = 5;
pub const MSG_SET_PEER_BW: u8 = 6;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
pub const MSG_AMF0_DATA: u8 = 18;
pub const MSG_AMF0_COMMAND: u8 = 20;

#[derive(Debug, Clone)]
pub enum RtmpMessage {
    SetChunkSize(u32),
    Abort(u32),
    Ack(u32),
    UserControl(u16, Vec<u8>),
    WindowAckSize(u32),
    SetPeerBandwidth(u32, u8),
    Audio { stream_id: u32, timestamp: u32, data: Bytes },
    Video { stream_id: u32, timestamp: u32, data: Bytes },
    Amf0Data { stream_id: u32, timestamp: u32, data: Bytes },
    Amf0Command { stream_id: u32, timestamp: u32, data: Bytes },
}

impl RtmpMessage {
    pub fn type_id(&self) -> u8 {
        match self {
            RtmpMessage::SetChunkSize(_) => MSG_SET_CHUNK_SIZE,
            RtmpMessage::Abort(_) => MSG_ABORT,
            RtmpMessage::Ack(_) => MSG_ACK,
            RtmpMessage::UserControl(_, _) => MSG_USER_CONTROL,
            RtmpMessage::WindowAckSize(_) => MSG_WINDOW_ACK_SIZE,
            RtmpMessage::SetPeerBandwidth(_, _) => MSG_SET_PEER_BW,
            RtmpMessage::Audio { .. } => MSG_AUDIO,
            RtmpMessage::Video { .. } => MSG_VIDEO,
            RtmpMessage::Amf0Data { .. } => MSG_AMF0_DATA,
            RtmpMessage::Amf0Command { .. } => MSG_AMF0_COMMAND,
        }
    }

    pub fn stream_id(&self) -> u32 {
        match self {
            RtmpMessage::Audio { stream_id, .. }
            | RtmpMessage::Video { stream_id, .. }
            | RtmpMessage::Amf0Data { stream_id, .. }
            | RtmpMessage::Amf0Command { stream_id, .. } => *stream_id,
            _ => 0,
        }
    }

    pub fn timestamp(&self) -> u32 {
        match self {
            RtmpMessage::Audio { timestamp, .. }
            | RtmpMessage::Video { timestamp, .. }
            | RtmpMessage::Amf0Data { timestamp, .. }
            | RtmpMessage::Amf0Command { timestamp, .. } => *timestamp,
            _ => 0,
        }
    }

    pub fn payload(&self) -> Bytes {
        match self {
            RtmpMessage::SetChunkSize(size) => {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(*size);
                buf.freeze()
            }
            RtmpMessage::Abort(stream_id) => {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(*stream_id);
                buf.freeze()
            }
            RtmpMessage::Ack(seq) => {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(*seq);
                buf.freeze()
            }
            RtmpMessage::UserControl(event_type, data) => {
                let mut buf = BytesMut::with_capacity(2 + data.len());
                buf.put_u16(*event_type);
                buf.extend_from_slice(data);
                buf.freeze()
            }
            RtmpMessage::WindowAckSize(size) => {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(*size);
                buf.freeze()
            }
            RtmpMessage::SetPeerBandwidth(size, limit_type) => {
                let mut buf = BytesMut::with_capacity(5);
                buf.put_u32(*size);
                buf.put_u8(*limit_type);
                buf.freeze()
            }
            RtmpMessage::Audio { data, .. }
            | RtmpMessage::Video { data, .. }
            | RtmpMessage::Amf0Data { data, .. }
            | RtmpMessage::Amf0Command { data, .. } => data.clone(),
        }
    }
}

#[derive(Clone)]
struct ChunkState {
    is_abs_timestamp: bool,
    has_extended_ts: bool,
    ts_field: u32,
    timestamp: u32,
    msg_length: u32,
    msg_type_id: u8,
    msg_stream_id: u32,
    buffer: BytesMut,
}

pub struct RtmpMessageParser {
    chunk_size: usize,
    chunk_states: std::collections::HashMap<u32, ChunkState>,
    buffer: BytesMut,
}

impl RtmpMessageParser {
    pub fn new() -> Self {
        Self {
            chunk_size: 128,
            chunk_states: std::collections::HashMap::new(),
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub fn set_chunk_size(&mut self, size: u32) {
        self.chunk_size = size as usize;
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<RtmpMessage> {
        self.buffer.extend_from_slice(data);
        let mut messages = Vec::new();
        loop {
            let buf_len_before = self.buffer.len();
            match self.try_parse_one() {
                Ok(Some(msg)) => messages.push(msg),
                Ok(None) => {
                    if self.buffer.len() == buf_len_before {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        messages
    }

    fn try_parse_one(&mut self) -> anyhow::Result<Option<RtmpMessage>> {
        if self.buffer.is_empty() {
            return Ok(None);
        }

        let mut pos = 0;

        // Step 1: Parse basic header
        let basic_header = self.buffer[pos];
        let fmt = (basic_header >> 6) & 0x03;
        let csid_raw = basic_header & 0x3F;

        let (csid, basic_header_len) = match csid_raw {
            0 => {
                if self.buffer.len() < 2 { return Ok(None); }
                (self.buffer[1] as u32 + 64, 2)
            }
            1 => {
                if self.buffer.len() < 3 { return Ok(None); }
                let low = self.buffer[1] as u32;
                let high = self.buffer[2] as u32;
                (64 + low + high * 256, 3)
            }
            v => (v as u32, 1),
        };
        pos = basic_header_len;

        // Step 2: Parse message header based on fmt (with inheritance)
        let header_len = match fmt {
            0 => 11usize,
            1 => 7usize,
            2 => 3usize,
            3 => 0usize,
            _ => return Ok(None),
        };

        if self.buffer.len() < pos + header_len {
            return Ok(None);
        }

        let prev = self.chunk_states.get(&csid).cloned();

        // Create or restore chunk context (like ZLMediaKit's now_packet/last_packet)
        let mut state = if let Some(ref p) = prev {
            ChunkState {
                is_abs_timestamp: false,
                has_extended_ts: p.has_extended_ts,
                ts_field: p.ts_field,
                timestamp: p.timestamp,
                msg_length: p.msg_length,
                msg_type_id: p.msg_type_id,
                msg_stream_id: p.msg_stream_id,
                buffer: if fmt == 0 { BytesMut::new() } else { p.buffer.clone() },
            }
        } else {
            ChunkState {
                is_abs_timestamp: false,
                has_extended_ts: false,
                ts_field: 0,
                timestamp: 0,
                msg_length: 0,
                msg_type_id: 0,
                msg_stream_id: 0,
                buffer: BytesMut::new(),
            }
        };

        // Fall-through style: fmt=0 sets all, fmt=1 overrides subset, etc.
        match fmt {
            0 => {
                state.is_abs_timestamp = true;
                state.has_extended_ts = false;
                state.msg_stream_id = u32::from_le_bytes([
                    self.buffer[pos + 7],
                    self.buffer[pos + 8],
                    self.buffer[pos + 9],
                    self.buffer[pos + 10],
                ]);
                state.msg_type_id = self.buffer[pos + 6];
                state.msg_length = u32::from_be_bytes([
                    0, self.buffer[pos + 3],
                    self.buffer[pos + 4], self.buffer[pos + 5],
                ]);
                state.ts_field = u32::from_be_bytes([
                    0, self.buffer[pos],
                    self.buffer[pos + 1],
                    self.buffer[pos + 2],
                ]);
            }
            1 => {
                state.msg_type_id = self.buffer[pos + 6];
                state.msg_length = u32::from_be_bytes([
                    0, self.buffer[pos + 3],
                    self.buffer[pos + 4], self.buffer[pos + 5],
                ]);
                state.ts_field = u32::from_be_bytes([
                    0, self.buffer[pos],
                    self.buffer[pos + 1],
                    self.buffer[pos + 2],
                ]);
            }
            2 => {
                state.ts_field = u32::from_be_bytes([
                    0, self.buffer[pos],
                    self.buffer[pos + 1],
                    self.buffer[pos + 2],
                ]);
            }
            3 => {}
            _ => return Ok(None),
        }
        pos += header_len;

        // Step 3: Extended timestamp
        if state.ts_field == 0xFFFFFF {
            if self.buffer.len() < pos + 4 { return Ok(None); }
            state.ts_field = u32::from_be_bytes([
                self.buffer[pos], self.buffer[pos + 1],
                self.buffer[pos + 2], self.buffer[pos + 3],
            ]);
            state.has_extended_ts = true;
            pos += 4;
        } else if fmt == 3 && state.has_extended_ts {
            // fmt=3 continuation for a message that had extended ts
            if self.buffer.len() < pos + 4 { return Ok(None); }
            pos += 4; // skip the repeated extended ts value
        }

        // Step 4: Read up to chunk_size bytes of payload
        let remaining = state.msg_length.saturating_sub(state.buffer.len() as u32);
        let chunk_to_read = std::cmp::min(remaining, self.chunk_size as u32) as usize;
        let available = std::cmp::min(chunk_to_read, self.buffer.len() - pos);

        if available == 0 {
            return Ok(None);
        }

        state.buffer.extend_from_slice(&self.buffer[pos..pos + available]);
        pos += available;
        self.buffer.advance(pos);

        // Step 5: Check if message is complete
        if state.buffer.len() >= state.msg_length as usize {
            let ts = if state.is_abs_timestamp {
                state.ts_field
            } else {
                let base = prev.as_ref().map_or(0u32, |p| p.timestamp);
                base.wrapping_add(state.ts_field)
            };

            // Save context for future chunks on this csid
            let saved = ChunkState {
                is_abs_timestamp: state.is_abs_timestamp,
                has_extended_ts: state.has_extended_ts,
                ts_field: state.ts_field,
                timestamp: ts,
                msg_length: state.msg_length,
                msg_type_id: state.msg_type_id,
                msg_stream_id: state.msg_stream_id,
                buffer: BytesMut::new(),
            };
            self.chunk_states.insert(csid, saved);

            let payload = state.buffer.freeze();
            Ok(Some(self.make_message(ts, state.msg_length, state.msg_type_id, state.msg_stream_id, payload)))
        } else {
            let saved = ChunkState {
                is_abs_timestamp: state.is_abs_timestamp,
                has_extended_ts: state.has_extended_ts,
                ts_field: state.ts_field,
                timestamp: state.timestamp,
                msg_length: state.msg_length,
                msg_type_id: state.msg_type_id,
                msg_stream_id: state.msg_stream_id,
                buffer: state.buffer,
            };
            self.chunk_states.insert(csid, saved);
            Ok(None)
        }
    }

    fn make_message(&self, timestamp: u32, _msg_length: u32, msg_type_id: u8, msg_stream_id: u32, payload: Bytes) -> RtmpMessage {
        match msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else { 128 };
                RtmpMessage::SetChunkSize(size)
            }
            MSG_ABORT => {
                let sid = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else { 0 };
                RtmpMessage::Abort(sid)
            }
            MSG_ACK => {
                let seq = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else { 0 };
                RtmpMessage::Ack(seq)
            }
            MSG_USER_CONTROL => {
                let event_type = if payload.len() >= 2 {
                    u16::from_be_bytes([payload[0], payload[1]])
                } else { 0 };
                RtmpMessage::UserControl(event_type, payload.slice(2..).to_vec())
            }
            MSG_WINDOW_ACK_SIZE => {
                let size = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else { 2500000 };
                RtmpMessage::WindowAckSize(size)
            }
            MSG_SET_PEER_BW => {
                let size = if payload.len() >= 4 {
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                } else { 2500000 };
                let limit_type = if payload.len() >= 5 { payload[4] } else { 2 };
                RtmpMessage::SetPeerBandwidth(size, limit_type)
            }
            MSG_AUDIO => RtmpMessage::Audio { stream_id: msg_stream_id, timestamp, data: payload },
            MSG_VIDEO => RtmpMessage::Video { stream_id: msg_stream_id, timestamp, data: payload },
            MSG_AMF0_DATA => RtmpMessage::Amf0Data { stream_id: msg_stream_id, timestamp, data: payload },
            MSG_AMF0_COMMAND => RtmpMessage::Amf0Command { stream_id: msg_stream_id, timestamp, data: payload },
            _ => RtmpMessage::Amf0Data { stream_id: msg_stream_id, timestamp, data: payload },
        }
    }
}

impl Default for RtmpMessageParser {
    fn default() -> Self { Self::new() }
}

#[derive(Clone)]
pub struct RtmpMessageEncoder {
    chunk_size: usize,
}

impl RtmpMessageEncoder {
    pub fn new() -> Self { Self { chunk_size: 128 } }

    pub fn set_chunk_size(&mut self, size: usize) { self.chunk_size = size; }

    pub fn encode(&self, msg: &RtmpMessage) -> BytesMut {
        let mut buf = BytesMut::new();
        let payload = msg.payload();
        let type_id = msg.type_id();
        let stream_id = msg.stream_id();
        let timestamp = msg.timestamp();

        let csid = match type_id {
            MSG_AUDIO => 4,
            MSG_VIDEO => 6,
            MSG_AMF0_COMMAND => 3,
            MSG_AMF0_DATA => 8,
            _ => 2,
        };

        let basic_header = encode_basic_header(0, csid);
        buf.extend_from_slice(&basic_header);

        let ts_field = if timestamp >= 0xFFFFFF { 0xFFFFFF } else { timestamp };
        buf.put_u8(((ts_field >> 16) & 0xFF) as u8);
        buf.put_u8(((ts_field >> 8) & 0xFF) as u8);
        buf.put_u8((ts_field & 0xFF) as u8);
        buf.put_u8(((payload.len() >> 16) & 0xFF) as u8);
        buf.put_u8(((payload.len() >> 8) & 0xFF) as u8);
        buf.put_u8((payload.len() & 0xFF) as u8);
        buf.put_u8(type_id);
        buf.put_u8((stream_id & 0xFF) as u8);
        buf.put_u8(((stream_id >> 8) & 0xFF) as u8);
        buf.put_u8(((stream_id >> 16) & 0xFF) as u8);
        buf.put_u8(0);

        if timestamp >= 0xFFFFFF {
            buf.put_u32(timestamp);
        }

        let mut offset = 0;
        while offset < payload.len() {
            let chunk_end = std::cmp::min(offset + self.chunk_size, payload.len());
            buf.extend_from_slice(&payload[offset..chunk_end]);
            offset = chunk_end;
            if offset < payload.len() {
                buf.extend_from_slice(&encode_basic_header(3, csid));
            }
        }

        buf
    }
}

pub fn encode_basic_header(fmt: u8, csid: u32) -> BytesMut {
    let mut buf = BytesMut::new();
    if csid >= 64 + 256 {
        buf.put_u8((fmt << 6) | 1);
        let adjusted = csid - 64;
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

impl Default for RtmpMessageEncoder {
    fn default() -> Self { Self::new() }
}
