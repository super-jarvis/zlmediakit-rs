//! Optional AAC -> Opus audio transcoding for WebRTC playback.
//!
//! WebRTC has no AAC codec, so a stream published as AAC (e.g. via RTMP) cannot
//! be sent to a WHEP player directly. This module transcodes AAC to Opus.
//!
//! It is only compiled with the `transcode` Cargo feature because it needs
//! native libraries at build time. `libopus` is always required (the `opus`
//! crate vendors and builds it via CMake). The AAC decoder needs an external
//! AAC library: build with `--features aac-transcode`, which pulls in
//! `fdk-aac` (libfdk-aac). Install `libfdk-aac-dev` (and `cmake`) before
//! building with either feature.

use anyhow::{anyhow, Result};
use opus::{Application, Channels};

/// PCM samples (16-bit signed little-endian) exchanged with the codecs.
pub type Pcm = Vec<i16>;

/// Decodes a compressed audio frame into PCM. AAC (and any other source codec
/// WebRTC cannot carry) implements this.
pub trait Decoder: Send {
    fn decode(&mut self, frame: &[u8]) -> Result<Pcm>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u8;
    /// Feed the codec-specific decoder configuration (e.g. an AAC
    /// AudioSpecificConfig). Called once for the `config_frame` of a source.
    fn configure(&mut self, _config: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// Opus encoder wrapper around the `opus` crate (requires `libopus`).
pub struct OpusEncoder {
    enc: opus::Encoder,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch = if channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };
        let enc = opus::Encoder::new(sample_rate, ch, Application::Voip)?;
        Ok(Self { enc })
    }

    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        let out = self.enc.encode_vec(pcm, 4 * 1024)?;
        Ok(out)
    }
}

#[cfg(feature = "aac-transcode")]
pub struct FdkAacDecoder {
    dec: fdk_aac::dec::Decoder,
    sample_rate: u32,
    channels: u8,
}

#[cfg(feature = "aac-transcode")]
impl FdkAacDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        // Our AAC MediaFrames carry raw AAC (no ADTS wrapper); the first
        // `config_frame` provides the AudioSpecificConfig via `configure`.
        let mut dec = fdk_aac::dec::Decoder::new(fdk_aac::dec::Transport::Raw);
        let _ = dec.set_min_output_channels(channels as usize);
        let _ = dec.set_max_output_channels(channels as usize);
        Ok(Self {
            dec,
            sample_rate,
            channels,
        })
    }
}

#[cfg(feature = "aac-transcode")]
impl Decoder for FdkAacDecoder {
    fn configure(&mut self, config: &[u8]) -> Result<()> {
        self.dec
            .config_raw(config)
            .map_err(|e| anyhow!("fdk-aac config error: {:?}", e))?;
        Ok(())
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Pcm> {
        // Push the whole compressed frame into the decoder's internal buffer
        // (fill may consume it in chunks).
        let mut off = 0usize;
        while off < frame.len() {
            let n = self
                .dec
                .fill(&frame[off..])
                .map_err(|e| anyhow!("fdk-aac fill error: {:?}", e))?;
            if n == 0 {
                break;
            }
            off += n;
        }
        // Decode one complete frame. `decoded_frame_size()` is only reliable
        // *after* a frame has been decoded, so we allocate a generously sized
        // buffer up front (2048 samples/channel covers AAC-LC and HE-AAC) and
        // truncate to the true size afterwards.
        let buf_len = 2048 * self.channels.max(1) as usize;
        let mut pcm = vec![0i16; buf_len];
        self.dec
            .decode_frame(&mut pcm)
            .map_err(|e| anyhow!("fdk-aac decode error: {:?}", e))?;
        let valid = self.dec.decoded_frame_size();
        if valid > 0 && valid <= pcm.len() {
            pcm.truncate(valid);
        }
        Ok(pcm)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }
}

/// Chains an optional `Decoder` (AAC) with an `OpusEncoder`.
///
/// PCM produced by the decoder is buffered internally and cut into Opus-sized
/// frames. This is necessary because AAC-LC emits 1024 samples/channel while
/// Opus only accepts frame sizes of 120/240/480/960/1920/2880 samples at 48 kHz
/// (i.e. 2.5/5/10/20/40/60 ms). The leftover samples carry over to the next
/// `transcode` call, so the Opus output stays correctly clocked.
pub struct AudioTranscoder {
    decoder: Option<Box<dyn Decoder>>,
    opus: OpusEncoder,
    pcm_buf: Vec<i16>,
    frame_samples: usize,
}

impl AudioTranscoder {
    /// `decoder` is `None` when no AAC decoder feature is enabled; `transcode`
    /// then returns an error and the caller should skip the frame.
    pub fn new(
        decoder: Option<Box<dyn Decoder>>,
        opus_rate: u32,
        opus_channels: u8,
    ) -> Result<Self> {
        // 20 ms Opus frames at the encoder sample rate.
        let frame_samples = (opus_rate as usize * 20 / 1000).max(1) * opus_channels.max(1) as usize;
        Ok(Self {
            decoder,
            opus: OpusEncoder::new(opus_rate, opus_channels)?,
            pcm_buf: Vec::new(),
            frame_samples,
        })
    }

    /// Feed the AAC decoder configuration (AudioSpecificConfig).
    pub fn configure(&mut self, config: &[u8]) -> Result<()> {
        match &mut self.decoder {
            Some(d) => d.configure(config),
            None => Err(anyhow!("no AAC decoder available for transcoding")),
        }
    }

    /// Transcode a source audio frame into one or more Opus packets. More than
    /// one packet is produced when the accumulated PCM crosses an Opus frame
    /// boundary (e.g. when adapting AAC's 1024/ch to Opus's 960/ch).
    pub fn transcode(&mut self, source_data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let pcm = match &mut self.decoder {
            Some(d) => d.decode(source_data)?,
            None => return Err(anyhow!("no AAC decoder available for transcoding")),
        };
        self.pcm_buf.extend_from_slice(&pcm);
        let mut out = Vec::new();
        while self.pcm_buf.len() >= self.frame_samples {
            let chunk: Vec<i16> = self.pcm_buf.drain(..self.frame_samples).collect();
            out.push(self.opus.encode(&chunk)?);
        }
        Ok(out)
    }

    pub fn has_decoder(&self) -> bool {
        self.decoder.is_some()
    }
}

#[cfg(all(test, feature = "transcode"))]
mod tests {
    use super::*;

    #[test]
    fn opus_roundtrip_is_nonempty() {
        // Encode 20ms of silence at 48k stereo; must produce a packet.
        let mut enc = OpusEncoder::new(48_000, 2).unwrap();
        let pcm = vec![0i16; 48_000 * 2 * 20 / 1000];
        let pkt = enc.encode(&pcm).unwrap();
        assert!(!pkt.is_empty(), "opus encode should emit a packet");
    }
}

#[cfg(all(test, feature = "aac-transcode"))]
mod aac_tests {
    use super::*;
    use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};

    #[test]
    fn aac_to_opus_pipeline_nonempty() {
        let sample_rate = 48_000u32;
        let channels: u8 = 2;

        // Encode 20ms of PCM to raw AAC using the fdk-aac encoder. This gives
        // us a real AudioSpecificConfig to feed the decoder.
        let params = EncoderParams {
            audio_object_type: AudioObjectType::Mpeg4LowComplexity,
            bit_rate: BitRate::Cbr(128_000),
            sample_rate,
            transport: Transport::Raw,
            channels: if channels == 1 {
                ChannelMode::Mono
            } else {
                ChannelMode::Stereo
            },
        };
        let enc = Encoder::new(params).expect("create fdk-aac encoder");
        let info = enc.info().expect("query encoder info");
        let asc = &info.confBuf[..info.confSize as usize];
        assert!(!asc.is_empty(), "AudioSpecificConfig must be present");

        // One full AAC-LC frame: 1024 samples per channel.
        let frame_len = 1024usize * channels as usize;
        let pcm_in = vec![0i16; frame_len];
        let mut aac_buf = vec![0u8; 4096];
        let enc_info = enc.encode(&pcm_in, &mut aac_buf).expect("encode aac");
        assert!(enc_info.output_size > 0, "aac encoder must emit bytes");
        let aac_frame = &aac_buf[..enc_info.output_size];

        // Full pipeline: AAC -> PCM -> Opus.
        let mut transcoder = AudioTranscoder::new(
            Some(Box::new(
                FdkAacDecoder::new(sample_rate, channels).expect("create aac decoder"),
            )),
            sample_rate,
            channels,
        )
        .expect("create transcoder");
        transcoder.configure(asc).expect("configure aac decoder");
        let opus = transcoder
            .transcode(aac_frame)
            .expect("transcode aac->opus");
        assert!(
            !opus.is_empty() && opus.iter().all(|p| !p.is_empty()),
            "opus output must contain non-empty packets"
        );
    }
}
