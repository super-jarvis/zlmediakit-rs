pub struct G711Parser;

impl G711Parser {
    pub fn new() -> Self {
        Self
    }

    pub fn alaw_to_pcm(data: &[u8]) -> Vec<i16> {
        data.iter()
            .map(|&sample| Self::alaw_decode(sample))
            .collect()
    }

    pub fn ulaw_to_pcm(data: &[u8]) -> Vec<i16> {
        data.iter()
            .map(|&sample| Self::ulaw_decode(sample))
            .collect()
    }

    pub fn alaw_decode(sample: u8) -> i16 {
        let sample = sample ^ 0x55;
        let sign: i16 = if sample & 0x80 != 0 { -1 } else { 1 };
        let exponent = (sample >> 4) & 0x07;
        let mantissa = sample & 0x0F;
        let mut magnitude = ((mantissa << 1) | 1) << (exponent + 2);
        magnitude -= 0x84;
        sign * magnitude as i16
    }

    pub fn ulaw_decode(sample: u8) -> i16 {
        let sample = !sample;
        let sign: i16 = if sample & 0x80 != 0 { -1 } else { 1 };
        let exponent = (sample >> 4) & 0x07;
        let mantissa = sample & 0x0F;
        let mut magnitude = ((mantissa << 1) | 1) << (exponent + 2);
        magnitude -= 0x21;
        sign * magnitude as i16
    }

    pub fn pcm_to_alaw(sample: i16) -> u8 {
        let mut sign: i16 = 0;
        let mut sample = sample;
        if sample < 0 {
            sign = 0x80;
            sample = -sample;
        }
        if sample > 32635 {
            sample = 32635;
        }

        let mut exponent: i16 = 7;
        let mut mask: i16 = 0x4000;
        while exponent > 0 && (sample & mask) == 0 {
            exponent -= 1;
            mask >>= 1;
        }

        let mantissa = (sample >> (exponent + 3)) & 0x0F;
        let encoded = sign | (exponent << 4) | mantissa;
        (encoded ^ 0x55) as u8
    }

    pub fn pcm_to_ulaw(sample: i16) -> u8 {
        let mut sign: i16 = 0x80;
        let mut sample = sample;
        if sample < 0 {
            sign = 0;
            sample = -sample;
        }
        if sample > 32635 {
            sample = 32635;
        }
        sample += 0x84;

        let mut exponent: i16 = 7;
        let mut mask: i16 = 0x4000;
        while exponent > 0 && (sample & mask) == 0 {
            exponent -= 1;
            mask >>= 1;
        }

        let mantissa = (sample >> (exponent + 3)) & 0x0F;
        (!(sign | (exponent << 4) | mantissa)) as u8
    }
}

impl Default for G711Parser {
    fn default() -> Self {
        Self::new()
    }
}
