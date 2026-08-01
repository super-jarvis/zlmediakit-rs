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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_annexb_empty() {
        let nals = H264Parser::parse_annexb(b"");
        assert!(nals.is_empty());
    }

    #[test]
    fn parse_annexb_no_startcode() {
        let nals = H264Parser::parse_annexb(b"no start code here");
        assert!(nals.is_empty());
    }

    #[test]
    fn parse_annexb_single_nal() {
        let data = b"\x00\x00\x01\x67\x42\x00\x1e\xa6";
        let nals = H264Parser::parse_annexb(data);
        assert_eq!(nals.len(), 1);
        assert_eq!(&nals[0][..], b"\x67\x42\x00\x1e\xa6");
    }

    #[test]
    fn parse_annexb_multiple_nals() {
        let data = b"\x00\x00\x01\x67\x42\x00\x1e\xa6\x00\x00\x01\x68\xce\x3c\x80";
        let nals = H264Parser::parse_annexb(data);
        assert_eq!(nals.len(), 2);
        assert_eq!(&nals[0][..], b"\x67\x42\x00\x1e\xa6");
        assert_eq!(&nals[1][..], b"\x68\xce\x3c\x80");
    }

    #[test]
    fn parse_annexb_four_byte_startcode() {
        let data = b"\x00\x00\x00\x01\x67\x42\x00\x1e";
        let nals = H264Parser::parse_annexb(data);
        assert_eq!(nals.len(), 1);
        assert_eq!(&nals[0][..], b"\x67\x42\x00\x1e");
    }

    #[test]
    fn parse_annexb_mixed_startcodes() {
        let data = b"\x00\x00\x01\x67\x00\x00\x00\x01\x68\x00\x00\x01\x65";
        let nals = H264Parser::parse_annexb(data);
        assert_eq!(nals.len(), 3);
    }

    #[test]
    fn is_keyframe_idr() {
        assert!(H264Parser::is_keyframe(0x65)); // 5 = IDR
    }

    #[test]
    fn is_keyframe_sps() {
        assert!(H264Parser::is_keyframe(0x67)); // 7 = SPS
    }

    #[test]
    fn is_keyframe_pps() {
        assert!(H264Parser::is_keyframe(0x68)); // 8 = PPS
    }

    #[test]
    fn is_not_keyframe() {
        assert!(!H264Parser::is_keyframe(0x61)); // 1 = non-IDR
    }

    #[test]
    fn parse_sps_valid() {
        let sps = b"\x67\x42\x00\x1e\xa6\x01\xe0\x80";
        let info = H264Parser::parse_sps(sps).unwrap();
        assert!(info.sps.is_some());
        // width = (data[3] << 8) | data[4] = (0x1e << 8) | 0xa6 = 0x1ea6
        assert_eq!(info.width, 0x1ea6);
        // height = (data[5] << 8) | data[6] = (0x01 << 8) | 0xe0 = 0x01e0
        assert_eq!(info.height, 0x01e0);
    }

    #[test]
    fn parse_sps_not_sps() {
        let non_sps = b"\x65\x42\x00\x1e";
        assert!(H264Parser::parse_sps(non_sps).is_none());
    }

    #[test]
    fn parse_sps_empty() {
        assert!(H264Parser::parse_sps(b"").is_none());
    }

    #[test]
    fn extract_config_found() {
        let data = b"\x00\x00\x01\x67\x42\x00\x1e\xa6\x00\x00\x01\x68\xce\x3c\x80";
        let (sps, pps) = H264Parser::extract_config(data).unwrap();
        assert_eq!(&sps[..], b"\x67\x42\x00\x1e\xa6");
        assert_eq!(&pps[..], b"\x68\xce\x3c\x80");
    }

    #[test]
    fn extract_config_missing_sps() {
        let data = b"\x00\x00\x01\x68\xce\x3c\x80";
        assert!(H264Parser::extract_config(data).is_none());
    }

    #[test]
    fn extract_config_missing_pps() {
        let data = b"\x00\x00\x01\x67\x42\x00\x1e";
        assert!(H264Parser::extract_config(data).is_none());
    }

    #[test]
    fn extract_config_empty() {
        assert!(H264Parser::extract_config(b"").is_none());
    }
}
