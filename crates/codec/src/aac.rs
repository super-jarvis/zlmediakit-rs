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
        header[3] =
            ((config.channel_config & 0x03) << 6) | (((frame_length >> 11) & 0x03) as u8) | 0x01;
        header[4] = ((frame_length >> 3) & 0xFF) as u8;
        header[5] = (((frame_length & 0x07) << 5) as u8) | 0x1F;
        header[6] = 0xFC;

        header
    }
}

impl Default for AacParser {
    fn default() -> Self {
        Self::new()
    }
}
