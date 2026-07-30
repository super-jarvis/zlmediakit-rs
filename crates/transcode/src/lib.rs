//! Video transcoding via ffmpeg subprocess.
//!
//! Supports H.264 ↔ H.265 re-encoding with optional resolution scaling
//! and bitrate control. Each `VideoTranscoder` spawns a long-running
//! ffmpeg process with stdin/stdout pipes.
//!
//! # Data format
//! - **Input**: Annex-B byte-stream (start-code delimited NAL units).
//!   Callers must convert from AVCC/FLV before feeding frames.
//! - **Output**: Annex-B byte-stream, parsed back into individual NAL
//!   unit groups (one group per output access unit / frame).

use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use anyhow::Context;
use tracing::{debug, warn};

/// Configuration for a video transcode pipeline.
#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    /// Input codec: "h264" or "hevc" (ffmpeg format names).
    pub input_codec: String,
    /// Output codec: "h264" or "hevc".
    pub output_codec: String,
    /// Output width (None = keep input resolution).
    pub width: Option<u32>,
    /// Output height (None = keep input resolution).
    pub height: Option<u32>,
    /// Target video bitrate (e.g. "2M", "500k").
    pub bitrate: String,
    /// ffmpeg preset (default "ultrafast" for low-latency streaming).
    pub preset: String,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            input_codec: "h264".into(),
            output_codec: "h264".into(),
            width: None,
            height: None,
            bitrate: "2M".into(),
            preset: "ultrafast".into(),
        }
    }
}

/// A single output frame from the transcoder in Annex-B format.
#[derive(Debug, Clone)]
pub struct TranscodeOutput {
    /// Annex-B byte-stream for one access unit (may contain multiple NALUs).
    pub data: Vec<u8>,
    /// Whether this is a keyframe (IDR).
    pub key_frame: bool,
}

/// Orchestrates an ffmpeg subprocess that decodes, optionally scales,
/// and re-encodes a video stream.
///
/// **Usage pattern**: feed all input frames via `feed_frame()`, then
/// call `finish()` to close stdin and wait for ffmpeg to complete.
/// The accumulated output bytes can then be parsed into individual
/// access units.
pub struct VideoTranscoder {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    output_codec: String,
}

impl VideoTranscoder {
    /// Spawns an ffmpeg process and returns a ready-to-use transcoder.
    pub async fn new(config: &TranscodeConfig) -> anyhow::Result<Self> {
        let encoder = match config.output_codec.as_str() {
            "h264" | "H264" | "avc" => "libx264",
            "h265" | "H265" | "hevc" | "HEVC" => "libx265",
            other => anyhow::bail!("unsupported output codec: {}", other),
        };
        let output_format = match config.output_codec.as_str() {
            "h264" | "H264" | "avc" => "h264",
            "h265" | "H265" | "hevc" | "HEVC" => "hevc",
            other => anyhow::bail!("unsupported output codec: {}", other),
        };
        let input_format = match config.input_codec.as_str() {
            "h264" | "H264" | "avc" => "h264",
            "h265" | "H265" | "hevc" | "HEVC" => "hevc",
            other => anyhow::bail!("unsupported input codec: {}", other),
        };

        let mut cmd = Command::new("ffmpeg");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .arg("-loglevel")
            .arg("warning")
            .arg("-f")
            .arg(input_format)
            .arg("-i")
            .arg("pipe:0")
            .arg("-c:v")
            .arg(encoder)
            .arg("-preset")
            .arg(&config.preset)
            .arg("-tune")
            .arg("zerolatency")
            .arg("-b:v")
            .arg(&config.bitrate);

        // Scale filter if requested
        if let (Some(w), Some(h)) = (config.width, config.height) {
            cmd.arg("-vf")
                .arg(format!("scale={}:{}", w, h));
        } else if let Some(w) = config.width {
            cmd.arg("-vf")
                .arg(format!("scale={}:-1", w));
        } else if let Some(h) = config.height {
            cmd.arg("-vf")
                .arg(format!("scale=-1:{}", h));
        }

        cmd.arg("-f")
            .arg(output_format)
            .arg("pipe:1");

        debug!("spawning ffmpeg: {:?}", cmd);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().context("stdin pipe")?;
        let stdout = child.stdout.take().context("stdout pipe")?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            output_codec: config.output_codec.clone(),
        })
    }

    /// Feeds an encoded frame (Annex-B format) to ffmpeg's stdin.
    /// Call this for each input frame before calling `finish()`.
    pub async fn feed_frame(&mut self, annex_b_data: &[u8]) -> anyhow::Result<()> {
        if let Some(ref mut stdin) = self.stdin {
            if !annex_b_data.is_empty() {
                stdin.write_all(annex_b_data).await?;
            }
        }
        Ok(())
    }

    /// Closes stdin and waits for ffmpeg to complete. Returns all
    /// stdout bytes (Annex-B encoded output).
    pub async fn finish(&mut self) -> anyhow::Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;

        // Close stdin to signal EOF to ffmpeg
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush().await;
            let _ = stdin.shutdown().await;
            drop(stdin);
        }

        // Read all stdout output
        let mut output = Vec::new();
        if let Some(mut stdout) = self.stdout.take() {
            stdout.read_to_end(&mut output).await?;
        }

        // Wait for ffmpeg to finish
        let status = self.child.wait().await?;
        if !status.success() {
            warn!("ffmpeg exited with {}", status);
        }

        Ok(output)
    }

    /// Feeds frames and finishes — convenience method for batch transcoding.
    pub async fn transcode_all(
        mut self,
        annex_b_data: &[u8],
    ) -> anyhow::Result<Vec<TranscodeOutput>> {
        self.feed_frame(annex_b_data).await?;
        let output = self.finish().await?;
        Ok(parse_annex_b_frames(&output, &self.output_codec))
    }

    /// Shuts down the ffmpeg process.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stdin.take();
        let _ = self.child.kill().await;
        Ok(())
    }
}

/// Parses Annex-B byte-stream output into individual frames (access units).
pub fn parse_annex_b_frames(data: &[u8], codec: &str) -> Vec<TranscodeOutput> {
    let mut frames = Vec::new();
    let mut pos = 0;
    let len = data.len();

    while pos < len {
        // Find next start code
        let sc_pos = match find_start_code(&data[pos..]) {
            Some(p) => pos + p,
            None => break,
        };

        // Determine start code length
        let sc_len = if sc_pos + 4 <= len && data[sc_pos..sc_pos + 4] == [0x00, 0x00, 0x00, 0x01] {
            4
        } else {
            3
        };

        // Find the NEXT start code after this one
        let search_start = sc_pos + sc_len;
        let next_sc = find_start_code(&data[search_start..]).map(|p| search_start + p);

        let frame_data = match next_sc {
            Some(end) => data[sc_pos..end].to_vec(),
            None => data[sc_pos..].to_vec(), // last frame
        };

        let key_frame = contains_idr(&frame_data, codec);
        frames.push(TranscodeOutput {
            data: frame_data,
            key_frame,
        });

        match next_sc {
            Some(end) => pos = end,
            None => break,
        }
    }

    frames
}

impl Drop for VideoTranscoder {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            // Child still running — send kill and spin-wait to reap.
            let _ = self.child.start_kill();
            for _ in 0..50 {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

// ── annex‑B / AVCC conversion ──────────────────────────────────────

/// Converts an FLV/AVCC video packet to Annex-B byte-stream format
/// suitable for feeding to ffmpeg.
///
/// FLV video packet layout:
///   [0] frame_type(4) | codec_id(4)
///   [1] avc_packet_type | composition_time(3)  (only for AVC/HEVC)
///   [5..] AVCC data (length-prefixed NAL units)
///
/// Returns empty vec for non-NALU packets (config frames, empty, etc.)
pub fn flv_video_to_annex_b(data: &[u8]) -> Vec<u8> {
    if data.len() < 2 {
        return Vec::new();
    }
    let avc_packet_type = data[1] & 0x0F;
    if avc_packet_type != 1 {
        // Config frame or other — skip (caller should handle config separately)
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pos = 5; // skip FLV tag header (1 + 1 + 3)
    while pos + 4 <= data.len() {
        let nalu_len = u32::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
        ]) as usize;
        pos += 4;
        if pos + nalu_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }
    out
}

/// Extracts SPS/PPS from an FLV config frame into Annex-B format.
/// Returns `(sps_pps_annex_b, sps_only, pps_only)`.
pub fn config_frame_to_annex_b(data: &[u8]) -> Vec<u8> {
    if data.len() < 5 {
        return Vec::new();
    }
    // FLV config: [codec_id, avc_packet_type=0, comp_time(3), AVCDecoderConfigurationRecord...]
    // AVCDecoderConfigurationRecord starts at offset 5
    if data[1] != 0 {
        return Vec::new();
    }
    let cfg = &data[5..];
    avcc_config_to_annex_b(cfg)
}

/// Converts an AVCDecoderConfigurationRecord (without FLV header) to
/// Annex-B SPS/PPS start-code format.
pub fn avcc_config_to_annex_b(cfg: &[u8]) -> Vec<u8> {
    if cfg.len() < 7 {
        return Vec::new();
    }
    // AVCDecoderConfigurationRecord:
    //   [0] version (1)
    //   [1] profile_indication
    //   [2] profile_compatibility
    //   [3] level_indication
    //   [4] lengthSizeMinusOne (lower 2 bits)
    //   [5] numOfSequenceParameterSets (lower 5 bits)
    let num_sps = (cfg[5] & 0x1F) as usize;
    let mut pos = 6;
    let mut out = Vec::new();

    for _ in 0..num_sps {
        if pos + 2 > cfg.len() {
            break;
        }
        let sps_len = u16::from_be_bytes([cfg[pos], cfg[pos + 1]]) as usize;
        pos += 2;
        if pos + sps_len > cfg.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&cfg[pos..pos + sps_len]);
        pos += sps_len;
    }

    if pos >= cfg.len() {
        return out;
    }
    let num_pps = cfg[pos] as usize;
    pos += 1;

    for _ in 0..num_pps {
        if pos + 2 > cfg.len() {
            break;
        }
        let pps_len = u16::from_be_bytes([cfg[pos], cfg[pos + 1]]) as usize;
        pos += 2;
        if pos + pps_len > cfg.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&cfg[pos..pos + pps_len]);
        pos += pps_len;
    }

    out
}

// ── annex‑B parsing helpers ─────────────────────────────────────────

/// Finds the next Annex-B start code (0x00000001 or 0x000001) in `data`.
fn find_start_code(data: &[u8]) -> Option<usize> {
    let len = data.len();
    for i in 0..len.saturating_sub(2) {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 3 < len && data[i + 2] == 1 {
                return Some(i);
            }
            if i + 4 <= len && i + 3 < len && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
    }
    None
}

/// Checks if an Annex-B byte-stream contains an IDR (keyframe) NAL unit.
fn contains_idr(data: &[u8], codec: &str) -> bool {
    match codec {
        "h264" | "H264" | "avc" => {
            // H.264 IDR: nal_unit_type == 5
            // After start code, first byte: forbidden(1)|priority(2)|type(5)
            for window in data.windows(5) {
                if (window[0] == 0 && window[1] == 0 && window[2] == 1)
                    || (window[0] == 0 && window[1] == 0 && window[2] == 0 && window[3] == 1)
                {
                    let nal_type = if window[2] == 1 {
                        window[3] & 0x1F
                    } else {
                        window[4] & 0x1F
                    };
                    if nal_type == 5 {
                        return true;
                    }
                }
            }
            false
        }
        "h265" | "H265" | "hevc" | "HEVC" => {
            // H.265 IRAP: nal_unit_type >= 16 && nal_unit_type <= 21
            // (BLA, IDR, CRA). The NAL unit type is in the lower 6 bits
            // of the first byte after the start code.
            for window in data.windows(4) {
                if (window[0] == 0 && window[1] == 0 && window[2] == 1)
                    || (window[0] == 0 && window[1] == 0 && window[2] == 0 && window[3] == 1)
                {
                    let b = if window[2] == 1 { window[3] } else { window[3] };
                    let nal_type = (b >> 1) & 0x3F;
                    if (16..=21).contains(&nal_type) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_start_code_basic() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        assert_eq!(find_start_code(&data), Some(0));

        let data2 = [0x00, 0x00, 0x01, 0x09, 0x10];
        assert_eq!(find_start_code(&data2), Some(0));

        let data3 = [0x01, 0x02, 0x00, 0x00, 0x01, 0x03];
        assert_eq!(find_start_code(&data3), Some(2));
    }

    #[test]
    fn contains_idr_h264() {
        // IDR NAL: forbidden=0, priority=0, type=5 → 0x25
        let data = [0x00, 0x00, 0x00, 0x01, 0x25, 0x00, 0x00];
        assert!(contains_idr(&data, "h264"));

        // Non-IDR: type=1 → 0x21
        let data2 = [0x00, 0x00, 0x00, 0x01, 0x21, 0x00, 0x00];
        assert!(!contains_idr(&data2, "h264"));
    }

    #[test]
    fn flv_to_annex_b_conversion() {
        // FLV AVC packet (packet_type=1): frame_type(1)|codec(7)=0x17,
        // packet_type=1, composition_time=0, then AVCC data
        let flv = [
            0x17, 0x01, 0x00, 0x00, 0x00, // FLV header
            0x00, 0x00, 0x00, 0x05, // NALU len=5
            0x09, 0x10, 0x11, 0x12, 0x13, // NALU data
        ];
        let annex_b = flv_video_to_annex_b(&flv);
        assert_eq!(
            annex_b,
            vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x11, 0x12, 0x13]
        );
    }

    #[test]
    fn flv_to_annex_b_skips_config() {
        // Packet type != 1 → empty
        let flv = [0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03];
        assert!(flv_video_to_annex_b(&flv).is_empty());
    }

    #[test]
    fn avcc_config_to_annex_b_basic() {
        // Simple AVCDecoderConfigurationRecord with 1 SPS, 1 PPS
        let sps = vec![0x67, 0x64, 0x00, 0x0a];
        let pps = vec![0x68, 0xee, 0x3c, 0x80];
        let mut cfg = vec![
            0x01, // version
            0x64, // profile
            0x00, // compat
            0x0a, // level
            0xFF, // lengthSizeMinusOne=3, reserved
            0xE1, // numSPS=1, reserved
        ];
        cfg.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        cfg.extend_from_slice(&sps);
        cfg.push(0x01); // numPPS=1
        cfg.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        cfg.extend_from_slice(&pps);

        let annex_b = avcc_config_to_annex_b(&cfg);
        let expected = {
            let mut e = Vec::new();
            e.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            e.extend_from_slice(&sps);
            e.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            e.extend_from_slice(&pps);
            e
        };
        assert_eq!(annex_b, expected);
    }

    #[test]
    fn transcode_config_default() {
        let cfg = TranscodeConfig::default();
        assert_eq!(cfg.input_codec, "h264");
        assert_eq!(cfg.output_codec, "h264");
        assert_eq!(cfg.bitrate, "2M");
        assert_eq!(cfg.preset, "ultrafast");
        assert!(cfg.width.is_none());
        assert!(cfg.height.is_none());
    }
}
