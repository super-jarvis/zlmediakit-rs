use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct H265Info {
    pub vps: Option<Bytes>,
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

pub struct H265Parser;

impl H265Parser {
    pub fn new() -> Self {
        Self
    }

    pub fn parse_annexb(data: &[u8]) -> Vec<Bytes> {
        let mut nals = Vec::new();
        let mut i = 0;
        let mut start = None;

        while i < data.len() {
            if i + 4 <= data.len() && &data[i..i + 4] == b"\x00\x00\x00\x01" {
                if let Some(s) = start {
                    nals.push(Bytes::copy_from_slice(&data[s..i]));
                }
                start = Some(i + 4);
                i += 4;
            } else if i + 3 <= data.len() && &data[i..i + 3] == b"\x00\x00\x01" {
                if let Some(s) = start {
                    nals.push(Bytes::copy_from_slice(&data[s..i]));
                }
                start = Some(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }

        if let Some(s) = start {
            if s < data.len() {
                nals.push(Bytes::copy_from_slice(&data[s..]));
            }
        }

        nals
    }

    pub fn is_keyframe(nal_type: u8) -> bool {
        let nalu_type = (nal_type >> 1) & 0x3F;
        matches!(nalu_type, 16..=21)
    }

    pub fn extract_config(data: &[u8]) -> Option<(Bytes, Bytes, Bytes)> {
        let nals = Self::parse_annexb(data);
        let mut vps = None;
        let mut sps = None;
        let mut pps = None;

        for nal in &nals {
            if nal.is_empty() {
                continue;
            }
            let nalu_type = (nal[0] >> 1) & 0x3F;
            match nalu_type {
                32 => vps = Some(nal.clone()),
                33 => sps = Some(nal.clone()),
                34 => pps = Some(nal.clone()),
                _ => {}
            }
        }

        match (vps, sps, pps) {
            (Some(v), Some(s), Some(p)) => Some((v, s, p)),
            _ => None,
        }
    }
}

impl Default for H265Parser {
    fn default() -> Self {
        Self::new()
    }
}
