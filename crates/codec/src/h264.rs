use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct H264Info {
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

pub struct H264Parser;

impl H264Parser {
    pub fn new() -> Self {
        Self
    }

    pub fn parse_annexb(data: &[u8]) -> Vec<Bytes> {
        let mut nals = Vec::new();
        let mut i = 0;
        let mut start = None;

        while i < data.len() {
            if i + 3 <= data.len() && &data[i..i + 3] == b"\x00\x00\x01" {
                if let Some(s) = start {
                    nals.push(Bytes::copy_from_slice(&data[s..i]));
                }
                start = Some(i + 3);
                i += 3;
            } else if i + 4 <= data.len() && &data[i..i + 4] == b"\x00\x00\x00\x01" {
                if let Some(s) = start {
                    nals.push(Bytes::copy_from_slice(&data[s..i]));
                }
                start = Some(i + 4);
                i += 4;
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
        let nalu_type = nal_type & 0x1F;
        nalu_type == 5 || nalu_type == 7 || nalu_type == 8
    }

    pub fn parse_sps(data: &[u8]) -> Option<H264Info> {
        if data.is_empty() {
            return None;
        }

        let nalu_type = data[0] & 0x1F;
        if nalu_type != 7 {
            return None;
        }

        let mut info = H264Info {
            sps: Some(Bytes::copy_from_slice(data)),
            pps: None,
            width: 0,
            height: 0,
            fps: 30.0,
        };

        if data.len() > 4 {
            let _profile_idc = data[1];
            let width = ((data[3] as u32) << 8) | (data[4] as u32);
            let height = ((data[5] as u32) << 8) | (data[6] as u32);
            info.width = width;
            info.height = height;
            info.fps = 30.0;
        }

        Some(info)
    }

    pub fn extract_config(data: &[u8]) -> Option<(Bytes, Bytes)> {
        let nals = Self::parse_annexb(data);
        let mut sps = None;
        let mut pps = None;

        for nal in &nals {
            if nal.is_empty() {
                continue;
            }
            let nalu_type = nal[0] & 0x1F;
            match nalu_type {
                7 => sps = Some(nal.clone()),
                8 => pps = Some(nal.clone()),
                _ => {}
            }
        }

        match (sps, pps) {
            (Some(s), Some(p)) => Some((s, p)),
            _ => None,
        }
    }
}

impl Default for H264Parser {
    fn default() -> Self {
        Self::new()
    }
}
