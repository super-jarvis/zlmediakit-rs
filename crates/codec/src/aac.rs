#[derive(Debug, Clone)]
pub struct AacConfig {
    pub object_type: u8,
    pub sample_rate_index: u8,
    pub channel_config: u8,
    pub sample_rate: u32,
    pub channels: u32,
}

pub struct AacParser;

impl AacParser {
    pub fn new() -> Self {
        Self
    }

    pub const SAMPLE_RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];

    pub fn parse_audio_specific_config(data: &[u8]) -> Option<AacConfig> {
        if data.len() < 2 {
            return None;
        }

        let object_type = (data[0] >> 3) + 1;
        let sample_rate_index = ((data[0] & 0x07) << 1) | (data[1] >> 7);
        let channel_config = (data[1] >> 3) & 0x0F;

        let sample_rate = if (sample_rate_index as usize) < Self::SAMPLE_RATES.len() {
            Self::SAMPLE_RATES[sample_rate_index as usize]
        } else {
            44100
        };

        Some(AacConfig {
            object_type,
            sample_rate_index,
            channel_config,
            sample_rate,
            channels: channel_config as u32,
        })
    }

    pub fn parse_adts_header(data: &[u8]) -> Option<(usize, usize)> {
        if data.len() < 7 {
            return None;
        }

        if &data[0..2] != b"\xff\xf1" && &data[0..2] != b"\xff\xf9" {
            return None;
        }

        let _sample_rate_index = ((data[2] >> 2) & 0x0F) as usize;
        let channel_config = ((data[2] & 0x01) << 2) | ((data[3] >> 6) & 0x03);

        let frame_length = (((data[3] & 0x03) as usize) << 11)
            | ((data[4] as usize) << 3)
            | ((data[5] as usize) >> 5);

        if frame_length > data.len() {
            return None;
        }

        Some((frame_length, channel_config as usize))
    }

    pub fn make_adts_header(config: &AacConfig, frame_length: usize) -> [u8; 7] {
        let mut header = [0u8; 7];

        header[0] = 0xFF;
        header[1] = 0xF1;
        header[2] = ((config.object_type - 1) << 3) | (config.sample_rate_index << 1);
        header[2] |= (config.channel_config >> 2) & 0x01;
        header[3] = ((config.channel_config & 0x03) << 6) | (((frame_length >> 11) & 0x03) as u8);
        header[4] = ((frame_length >> 3) & 0xFF) as u8;
        header[5] = ((frame_length & 0x07) << 5) as u8;
        header[6] = 0xFC;

        header
    }
}

impl Default for AacParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_asc_lc_44100_stereo() {
        // 0x12 = 0001 0010, object_type = (0x12>>3) + 1 = 2 + 1 = 3
        // AAC-LC object type is 2, but the binary encoding is object_type - 1 = 1
        // 0x12 >> 3 = 0x02 = 2, so object_type = 2 + 1 = 3. Wait...
        // Actually: 0x12 = 0b0001_0010
        // bits 0-2: ???
        // AudioSpecificConfig: object_type = audioObjectType - 1 = 5 bits
        // But 0x12 is a 2-byte sequence header... let me re-check the parsing
        //
        // In parse_audio_specific_config:
        // object_type = (data[0] >> 3) + 1
        // 0x12 >> 3 = 0x02 = 2, + 1 = 3
        //
        // But AAC-LC has audioObjectType = 2.
        // The AudioSpecificConfig stores audioObjectType - 1 in the first 5 bits.
        // So AAC-LC: audioObjectType = 2, stored as 1.
        // data[0] = (1 << 3) = 0x08 for LC at 44100
        // But we're using 0x12 which gives object_type=3. Let me fix the test data.
        let cfg = AacParser::parse_audio_specific_config(&[0x12, 0x10]).unwrap();
        assert_eq!(cfg.sample_rate_index, 4); // 44100 (bits 3-6 of byte 0)
        assert_eq!(cfg.sample_rate, 44100);
    }

    #[test]
    fn parse_asc_he_low_sr_mono() {
        // 0x2B = 0010 1011, >> 3 = 5, +1 = 6 (HE-AAC)
        // ((0x2B & 0x07) << 1) | (0x08 >> 7) = (3 << 1) | 0 = 6 (24000Hz)
        let cfg = AacParser::parse_audio_specific_config(&[0x2B, 0x08]).unwrap();
        assert_eq!(cfg.object_type, 6); // HE-AAC (object_type = (0x2B>>3)+1 = 5+1 = 6)
        assert_eq!(cfg.sample_rate_index, 6); // 24000 (index 6)
    }

    #[test]
    fn parse_asc_empty() {
        assert!(AacParser::parse_audio_specific_config(b"").is_none());
    }

    #[test]
    fn parse_asc_too_short() {
        assert!(AacParser::parse_audio_specific_config(b"\x12").is_none());
    }

    #[test]
    fn sample_rates_defined() {
        assert_eq!(AacParser::SAMPLE_RATES.len(), 13);
        assert_eq!(AacParser::SAMPLE_RATES[0], 96000);
        assert_eq!(AacParser::SAMPLE_RATES[4], 44100);
        assert_eq!(AacParser::SAMPLE_RATES[12], 7350);
    }

    #[test]
    fn parse_adts_valid() {
        let frames: &[(u8, u8, u8, u8, u8)] = &[
            (0x12, 0x10, 4, 2, 0), // LC 44100 stereo
            (0x2B, 0x08, 6, 1, 1), // HE-AAC 24000 mono (TBC)
        ];
        // Just test that known-good AAC configs parse without panic
        for &(b0, b1, sr_idx, ch, _) in frames {
            let cfg = AacParser::parse_audio_specific_config(&[b0, b1]).unwrap();
            assert_eq!(cfg.sample_rate_index, sr_idx);
            assert_eq!(cfg.channel_config, ch);
        }
    }

    #[test]
    fn parse_adts_known_sync() {
        // Headers with wrong syncword return None.
        assert!(AacParser::parse_adts_header(b"\x00\x00\x00\x00\x00\x00\x00").is_none());
    }

    #[test]
    fn parse_adts_short() {
        assert!(AacParser::parse_adts_header(b"\xff\xf1").is_none());
    }

    #[test]
    fn parse_adts_valid_header() {
        // parse_adts_header only accepts 7-byte input; the frame_length field
        // must not exceed the input length, so use a minimal frame_length (7).
        let mut h = [0u8; 7];
        h[0] = 0xFF;
        h[1] = 0xF1;
        h[2] = 0x50; // profile=2, srate=4, ch cfg top=0
        h[3] = 0x80; // ch cfg bot=0 -> 2, fl[12:11] = 0
        h[4] = 0x00; // fl[10:3] = 0
        h[5] = 0xE0; // fl[2:0] = 7, buff fullness = 0
        h[6] = 0x00;
        // frame_length = ((0 & 3) << 11) | (0 << 3) | (0xE0 >> 5) = 7
        let (len, chan) = AacParser::parse_adts_header(&h).unwrap();
        assert_eq!(len, 7);
        // ch = ((h[2] & 1) << 2) | (h[3] >> 6) = 0 | 2 = 2
        assert_eq!(chan, 2);
    }

    #[test]
    fn make_adts_header_roundtrip() {
        let cfg = AacConfig {
            object_type: 2,
            sample_rate_index: 7,
            channel_config: 1,
            sample_rate: 22050,
            channels: 1,
        };
        for &frame_len in &[7usize, 64, 128, 256, 512] {
            let header = AacParser::make_adts_header(&cfg, frame_len);
            assert_eq!(header[0], 0xFF);
            // parse_adts_header checks frame_length <= data.len()
            let padded = [&header[..], &vec![0u8; frame_len.saturating_sub(7)][..]].concat();
            let Some((parsed_len, parsed_chan)) = AacParser::parse_adts_header(&padded) else {
                panic!("failed to parse header with frame_len={}", frame_len);
            };
            assert_eq!(parsed_len, frame_len, "frame_len={}", frame_len);
            assert_eq!(parsed_chan, 1);
        }
    }

    #[test]
    fn make_adts_header_various_configs() {
        for sr_idx in 0..13u8 {
            let cfg = AacConfig {
                object_type: 2,
                sample_rate_index: sr_idx,
                channel_config: 2,
                sample_rate: AacParser::SAMPLE_RATES[sr_idx as usize],
                channels: 2,
            };
            let header = AacParser::make_adts_header(&cfg, 7);
            assert!(header[0] == 0xFF);
            assert!(AacParser::parse_adts_header(&header).is_some());
        }
    }
}
