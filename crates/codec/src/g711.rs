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
        let mantissa = (sample & 0x0F) as i16;
        let magnitude = (((mantissa << 1) | 1) << (exponent + 2)) - 0x84;
        sign * magnitude
    }

    pub fn ulaw_decode(sample: u8) -> i16 {
        let sample = !sample;
        let sign: i16 = if sample & 0x80 != 0 { -1 } else { 1 };
        let exponent = (sample >> 4) & 0x07;
        let mantissa = (sample & 0x0F) as i16;
        let magnitude = (((mantissa << 1) | 1) << (exponent + 2)) - 0x21;
        sign * magnitude
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alaw_silence_encodes_to_0x55() {
        assert_eq!(G711Parser::pcm_to_alaw(0), 0x55);
    }

    #[test]
    fn alaw_decode_0x55_known() {
        // Internal 0x00: sign=1(+), seg=0, mant=0
        // magnitude = (1<<2)-132 = -128; result = 1 * (-128) = -128
        assert_eq!(G711Parser::alaw_decode(0x55), -128);
    }

    #[test]
    fn alaw_decode_0xd5_known() {
        // Internal 0x80: sign=-1(neg), seg=0, mant=0
        // magnitude = (1<<2)-132 = -128; result = -1 * (-128) = 128
        assert_eq!(G711Parser::alaw_decode(0xD5), 128);
    }

    #[test]
    fn alaw_encode_known_values() {
        assert_eq!(G711Parser::pcm_to_alaw(0), 0x55);
        assert_eq!(G711Parser::pcm_to_alaw(1000), 0x7A);
        assert_eq!(G711Parser::pcm_to_alaw(-1000), 0xFA);
    }

    #[test]
    fn alaw_decode_known_values() {
        assert_eq!(G711Parser::alaw_decode(0x55), -128);
        assert_eq!(G711Parser::alaw_decode(0xD5), 128);
        assert_eq!(G711Parser::alaw_decode(0x7A), 364);
        assert_eq!(G711Parser::alaw_decode(0xFA), -364);
    }

    #[test]
    fn alaw_table_returns_256_values() {
        let pcm = G711Parser::alaw_to_pcm(&(0..=255u8).collect::<Vec<_>>());
        assert_eq!(pcm.len(), 256);
    }

    #[test]
    fn ulaw_silence_encodes_to_0x7f() {
        // μ-law encoder: pcm(0) + 0x84 bias → highest set bit → enc → bitwise NOT
        assert_eq!(G711Parser::pcm_to_ulaw(0), 0x7F);
    }

    #[test]
    fn ulaw_decode_0x7f_known() {
        // Internal 0x80: sign=-1(neg), seg=0, mant=0
        // magnitude = (1<<2)-33 = -29; result = -1 * (-29) = 29
        assert_eq!(G711Parser::ulaw_decode(0x7F), 29);
    }

    #[test]
    fn ulaw_decode_0xff_known() {
        // Internal 0x00: sign=1(pos), seg=0, mant=0
        // magnitude = (1<<2)-33 = -29; result = 1 * (-29) = -29
        assert_eq!(G711Parser::ulaw_decode(0xFF), -29);
    }

    #[test]
    fn ulaw_encode_known_values() {
        assert_eq!(G711Parser::pcm_to_ulaw(0), 0x7F);
        assert_eq!(G711Parser::pcm_to_ulaw(1000), 0x4E);
        assert_eq!(G711Parser::pcm_to_ulaw(-1000), 0xCE);
    }

    #[test]
    fn ulaw_decode_known_values() {
        assert_eq!(G711Parser::ulaw_decode(0x7F), 29);
        assert_eq!(G711Parser::ulaw_decode(0xFF), -29);
    }

    #[test]
    fn ulaw_table_returns_256_values() {
        let pcm = G711Parser::ulaw_to_pcm(&(0..=255u8).collect::<Vec<_>>());
        assert_eq!(pcm.len(), 256);
    }

    #[test]
    fn alaw_decode_buffer() {
        let input = [0xD5u8, 0x15, 0x55];
        let pcm = G711Parser::alaw_to_pcm(&input);
        assert_eq!(pcm.len(), 3);
    }

    #[test]
    fn ulaw_decode_buffer() {
        let input = [0xFFu8, 0x00, 0x7F];
        let pcm = G711Parser::ulaw_to_pcm(&input);
        assert_eq!(pcm.len(), 3);
    }
}
