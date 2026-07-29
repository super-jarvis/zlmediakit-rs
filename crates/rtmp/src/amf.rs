use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Debug, Clone)]
pub enum AmfValue {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, AmfValue)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, AmfValue)>),
    StrictArray(Vec<AmfValue>),
    Date { time: f64, timezone: i16 },
}

pub struct AmfEncoder;

impl AmfEncoder {
    pub fn encode(values: &[AmfValue]) -> BytesMut {
        let mut buf = BytesMut::new();
        for value in values {
            Self::encode_value(&mut buf, value);
        }
        buf
    }

    fn encode_value(buf: &mut BytesMut, value: &AmfValue) {
        match value {
            AmfValue::Number(n) => {
                buf.put_u8(0x00);
                buf.put_f64(*n);
            }
            AmfValue::Boolean(b) => {
                buf.put_u8(0x01);
                buf.put_u8(if *b { 1 } else { 0 });
            }
            AmfValue::String(s) => {
                if s.len() > 0xFFFF {
                    buf.put_u8(0x0C);
                    buf.put_u32(s.len() as u32);
                } else {
                    buf.put_u8(0x02);
                    buf.put_u16(s.len() as u16);
                }
                buf.extend_from_slice(s.as_bytes());
            }
            AmfValue::Object(pairs) => {
                buf.put_u8(0x03);
                for (key, val) in pairs {
                    Self::encode_property(buf, key, val);
                }
                buf.put_u8(0x00);
                buf.put_u8(0x00);
                buf.put_u8(0x09);
            }
            AmfValue::Null => {
                buf.put_u8(0x05);
            }
            AmfValue::Undefined => {
                buf.put_u8(0x06);
            }
            AmfValue::EcmaArray(pairs) => {
                buf.put_u8(0x08);
                buf.put_u32(pairs.len() as u32);
                for (key, val) in pairs {
                    Self::encode_property(buf, key, val);
                }
                buf.put_u8(0x00);
                buf.put_u8(0x00);
                buf.put_u8(0x09);
            }
            _ => {}
        }
    }

    fn encode_property(buf: &mut BytesMut, key: &str, value: &AmfValue) {
        if key.len() > 0xFFFF {
            buf.put_u32(key.len() as u32);
        } else {
            buf.put_u16(key.len() as u16);
        }
        buf.extend_from_slice(key.as_bytes());
        Self::encode_value(buf, value);
    }
}

pub struct AmfDecoder;

impl AmfDecoder {
    pub fn decode(data: &[u8]) -> anyhow::Result<Vec<AmfValue>> {
        let mut values = Vec::new();
        let mut buf = Bytes::copy_from_slice(data);

        while buf.has_remaining() {
            let value = Self::decode_value(&mut buf)?;
            values.push(value);
        }

        Ok(values)
    }

    fn decode_value(buf: &mut Bytes) -> anyhow::Result<AmfValue> {
        if !buf.has_remaining() {
            anyhow::bail!("Unexpected end of AMF data");
        }

        let type_id = buf.get_u8();
        match type_id {
            0x00 => {
                if buf.remaining() < 8 {
                    anyhow::bail!("Not enough data for AMF number");
                }
                let n = buf.get_f64();
                Ok(AmfValue::Number(n))
            }
            0x01 => {
                if buf.remaining() < 1 {
                    anyhow::bail!("Not enough data for AMF boolean");
                }
                let b = buf.get_u8() != 0;
                Ok(AmfValue::Boolean(b))
            }
            0x02 => {
                if buf.remaining() < 2 {
                    anyhow::bail!("Not enough data for AMF string length");
                }
                let len = buf.get_u16() as usize;
                if buf.remaining() < len {
                    anyhow::bail!(
                        "Not enough data for AMF string (need {} bytes, have {})",
                        len,
                        buf.remaining()
                    );
                }
                let mut data = vec![0u8; len];
                buf.copy_to_slice(&mut data);
                let s = String::from_utf8_lossy(&data).to_string();
                Ok(AmfValue::String(s))
            }
            0x03 => {
                let pairs = Self::decode_object(buf)?;
                Ok(AmfValue::Object(pairs))
            }
            0x05 => Ok(AmfValue::Null),
            0x06 => Ok(AmfValue::Undefined),
            0x08 => {
                if buf.remaining() < 4 {
                    anyhow::bail!("Not enough data for AMF ecma array count");
                }
                let _len = buf.get_u32();
                let pairs = Self::decode_object(buf)?;
                Ok(AmfValue::EcmaArray(pairs))
            }
            _ => {
                anyhow::bail!("Unsupported AMF type: {}", type_id);
            }
        }
    }

    fn decode_object(buf: &mut Bytes) -> anyhow::Result<Vec<(String, AmfValue)>> {
        let mut pairs = Vec::new();

        loop {
            if buf.remaining() < 2 {
                break;
            }

            let key_len = buf.get_u16() as usize;
            if key_len == 0 {
                if buf.remaining() >= 1 {
                    let _marker = buf.get_u8();
                }
                break;
            }

            if buf.remaining() < key_len {
                break;
            }

            let mut key_data = vec![0u8; key_len];
            buf.copy_to_slice(&mut key_data);
            let key = String::from_utf8_lossy(&key_data).to_string();

            if !buf.has_remaining() {
                break;
            }

            let value = Self::decode_value(buf)?;
            pairs.push((key, value));
        }

        Ok(pairs)
    }
}
