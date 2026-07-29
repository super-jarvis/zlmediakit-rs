//! Optional AAC -> Opus audio transcoding for WebRTC playback.
//!
//! WebRTC has no AAC codec, so a stream published as AAC (e.g. via RTMP) cannot
//! be sent to a WHEP player directly. This module transcodes AAC to Opus.
//!
//! It is only compiled with the `transcode` Cargo feature because it needs
//! native libraries at build time: `libopus` (always) and an AAC decoder. The
//! default AAC decoder uses `libfdk-aac` via the `aac` crate; build with
//! `--features aac-transcode` and install `libfdk-aac-dev` first.

use anyhow::{anyhow, Result};

/// PCM samples (16-bit signed little-endian) exchanged with the codecs.
pub type Pcm = Vec<i16>;

/// Decodes a compressed audio frame into PCM. AAC (and any other source codec
/// WebRTC cannot carry) implements this.
pub trait Decoder: Send {
    fn decode(&mut self, frame: &[u8]) -> Result<Pcm>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u8;
}

/// Opus encoder wrapper around the `opus` crate (requires `libopus`).
pub struct OpusEncoder {
    enc: opus::Encoder,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let enc = opus::Encoder::new(sample_rate, channels, opus::Application::Voip)?;
        Ok(Self { enc })
    }

    pub fn encode(&self, pcm: &[i16]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; 4 * 1024];
        let n = self.enc.encode(pcm, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

#[cfg(feature = "aac-transcode")]
pub struct FdkAacDecoder {
    dec: aac::Decoder,
    sample_rate: u32,
    channels: u8,
}

#[cfg(feature = "aac-transcode")]
impl FdkAacDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        Ok(Self {
            dec: aac::Decoder::new(sample_rate as usize, channels as usize)?,
            sample_rate,
            channels,
        })
    }
}

#[cfg(feature = "aac-transcode")]
impl Decoder for FdkAacDecoder {
    fn decode(&mut self, frame: &[u8]) -> Result<Pcm> {
        Ok(self.dec.decode(frame)?)
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u8 {
        self.channels
    }
}

/// Chains an optional `Decoder` (AAC) with an `OpusEncoder`.
pub struct AudioTranscoder {
    decoder: Option<Box<dyn Decoder>>,
    opus: OpusEncoder,
}

impl AudioTranscoder {
    /// `decoder` is `None` when no AAC decoder feature is enabled; `transcode`
    /// then returns an error and the caller should skip the frame.
    pub fn new(
        decoder: Option<Box<dyn Decoder>>,
        opus_rate: u32,
        opus_channels: u8,
    ) -> Result<Self> {
        Ok(Self {
            decoder,
            opus: OpusEncoder::new(opus_rate, opus_channels)?,
        })
    }

    /// Transcode a source audio frame into an Opus packet.
    pub fn transcode(&mut self, source_data: &[u8]) -> Result<Vec<u8>> {
        let pcm = match &mut self.decoder {
            Some(d) => d.decode(source_data)?,
            None => return Err(anyhow!("no AAC decoder available for transcoding")),
        };
        self.opus.encode(&pcm)
    }

    pub fn has_decoder(&self) -> bool {
        self.decoder.is_some()
    }
}
