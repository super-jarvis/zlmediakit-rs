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
        let nalu_type = nal_type & 0x3F;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_annexb_empty() {
        assert!(H265Parser::parse_annexb(b"").is_empty());
    }

    #[test]
    fn parse_annexb_single_nal() {
        let data = b"\x00\x00\x01\x40\x01\x0c\x01\xff";
        let nals = H265Parser::parse_annexb(data);
        assert_eq!(nals.len(), 1);
    }

    #[test]
    fn parse_annexb_multiple() {
        let data = b"\x00\x00\x01\x40\x01\x0c\x00\x00\x01\x42\x01\x01";
        let nals = H265Parser::parse_annexb(data);
        assert_eq!(nals.len(), 2);
    }

    #[test]
    fn parse_annexb_four_byte_startcode() {
        let data = b"\x00\x00\x00\x01\x40\x01\x0c";
        let nals = H265Parser::parse_annexb(data);
        assert_eq!(nals.len(), 1);
    }

    #[test]
    fn is_keyframe_bla_wrap() {
        // NAL unit type 16 (BLA_W_LP) is a keyframe
        assert!(H265Parser::is_keyframe(16));
    }

    #[test]
    fn is_keyframe_idr() {
        // IDR_W_RADL = 19, IDR_R_DPL = 20
        assert!(H265Parser::is_keyframe(19));
        assert!(H265Parser::is_keyframe(20));
    }

    #[test]
    fn is_not_keyframe() {
        assert!(!H265Parser::is_keyframe(0)); // TRAIL_N
        assert!(!H265Parser::is_keyframe(1));
    }

    #[test]
    fn extract_config_found() {
        let vps = b"\x40\x01\x0c\x01\xff\xff\x01\x60\x00\x00\x03\x00\xb0\x00\x00\x03\x00\x00\x03\x00\x5d\xa0\x02\x80\x80\x2d\x20";
        let sps = b"\x42\x01\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f";
        let pps = b"\x44\x01\x02\x03";
        let mut data = Vec::new();
        data.extend_from_slice(b"\x00\x00\x01");
        data.extend_from_slice(vps);
        data.extend_from_slice(b"\x00\x00\x01");
        data.extend_from_slice(sps);
        data.extend_from_slice(b"\x00\x00\x01");
        data.extend_from_slice(pps);
        let (rv, rs, rp) = H265Parser::extract_config(&data).unwrap();
        assert_eq!(&rv[..2], b"\x40\x01");
        assert_eq!(&rs[..2], b"\x42\x01");
        assert_eq!(&rp[..2], b"\x44\x01");
    }

    #[test]
    fn extract_config_missing_vps() {
        let sps = b"\x42\x01\x01";
        let pps = b"\x44\x01\x02";
        let mut data = Vec::new();
        data.extend_from_slice(b"\x00\x00\x01");
        data.extend_from_slice(sps);
        data.extend_from_slice(b"\x00\x00\x01");
        data.extend_from_slice(pps);
        assert!(H265Parser::extract_config(&data).is_none());
    }

    #[test]
    fn extract_config_empty() {
        assert!(H265Parser::extract_config(b"").is_none());
    }
}

impl Default for H265Parser {
    fn default() -> Self {
        Self::new()
    }
}
