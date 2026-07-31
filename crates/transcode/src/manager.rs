//! Real-time (streaming) transcoding manager.
//!
//! Bridges ZLMediaKit's `addTranscode` API semantics to the ffmpeg-based
//! [`VideoTranscoder`]: a `TranscodeSession` subscribes to a source
//! [`MediaSource`], converts each incoming video frame to Annex-B, feeds it
//! to a long-lived ffmpeg process, and re-publishes the transcoded output as
//! a new `MediaSource` (default `{stream}_out`, overridable via `dst_app` /
//! `dst_stream`).
//!
//! The ffmpeg process is kept open for the lifetime of the session: stdin is
//! written continuously and stdout is drained by a dedicated background task
//! so the OS pipe never blocks.

use crate::{config_frame_to_annex_b, flv_video_to_annex_b, TranscodeConfig};
use anyhow::Context;
use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zlmediakit_core::{CodecId, FrameType, MediaFrame, MediaSource, MediaSourceManager};

/// Output of a streaming transcode session, exposed via the manager for API
/// reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscodeInfo {
    /// Unique key identifying this transcode instance:
    /// `{vhost}/{app}/{stream}/{name}`.
    pub key: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    /// Output app/stream where the transcoded feed is published.
    pub dst_app: String,
    pub dst_stream: String,
    pub output_codec: String,
    pub input_codec: String,
    /// Total input frames fed to ffmpeg so far.
    pub in_frames: u64,
    /// Total output frames produced by ffmpeg so far.
    pub out_frames: u64,
    pub created_at: i64,
}

/// A long-lived ffmpeg transcoder that reads Annex-B from stdin and emits
/// transcoded Annex-B bytes on an mpsc channel. Dropping it kills ffmpeg.
struct StreamingTranscoder {
    child: tokio::process::Child,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    out_rx: mpsc::Receiver<Vec<u8>>,
    _stdout_task: tokio::task::JoinHandle<()>,
}

impl StreamingTranscoder {
    async fn new(config: &TranscodeConfig) -> Result<Self> {
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

        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::new(&config.ffmpeg_bin);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .arg("-loglevel")
            .arg("error")
            .arg("-fflags")
            .arg("+flush_packets")
            .arg("-flags")
            .arg("low_delay")
            .arg("-i")
            .arg("pipe:0")
            .arg("-c:v")
            .arg(encoder)
            .arg("-preset")
            .arg(&config.preset)
            .arg("-tune")
            .arg("zerolatency")
            .arg("-b:v")
            .arg(&config.bitrate)
            .arg("-g")
            .arg("25")
            .arg("-pix_fmt")
            .arg("yuv420p");

        if let (Some(w), Some(h)) = (config.width, config.height) {
            cmd.arg("-vf").arg(format!("scale={}:{}", w, h));
        } else if let Some(w) = config.width {
            cmd.arg("-vf").arg(format!("scale={}:-1", w));
        } else if let Some(h) = config.height {
            cmd.arg("-vf").arg(format!("scale=-1:{}", h));
        }

        cmd.arg("-f").arg(output_format).arg("pipe:1");

        debug!("spawning streaming ffmpeg: {:?}", cmd);

        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().context("stdin pipe")?;
        let mut stdout = child.stdout.take().context("stdout pipe")?;

        // Channel to feed stdin from the session task.
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        // Channel to drain stdout to the session task.
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(64);

        // Writer task: pumps the stdin channel into ffmpeg's stdin.
        let writer = tokio::spawn(async move {
            while let Some(chunk) = stdin_rx.recv().await {
                if chunk.is_empty() {
                    break;
                }
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
            let _ = stdin.shutdown().await;
        });

        // Reader task: continuously drains ffmpeg's stdout so the pipe never
        // blocks, forwarding raw Annex-B chunks to the session task.
        let stdout_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if out_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            writer.abort();
        });

        Ok(Self {
            child,
            stdin_tx,
            out_rx,
            _stdout_task: stdout_task,
        })
    }

    /// Feeds one Annex-B chunk to ffmpeg's stdin.
    async fn feed(&self, annex_b: &[u8]) -> Result<()> {
        if annex_b.is_empty() {
            return Ok(());
        }
        self.stdin_tx.send(annex_b.to_vec()).await.ok();
        Ok(())
    }

    /// Receives the next raw Annex-B chunk from ffmpeg's stdout.
    async fn recv(&mut self) -> Option<Vec<u8>> {
        self.out_rx.recv().await
    }

    fn shutdown(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Drop for StreamingTranscoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One transcoding session: subscribes to a source, transcodes it, and
/// re-publishes the output as a new `MediaSource`.
pub struct TranscodeSession {
    pub key: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub dst_app: String,
    pub dst_stream: String,
    pub config: TranscodeConfig,
    pub source: Arc<MediaSource>,
    pub dest: Arc<MediaSource>,
    in_frames: Arc<AtomicU64>,
    out_frames: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    created_at: i64,
}

impl TranscodeSession {
    fn info(&self) -> TranscodeInfo {
        TranscodeInfo {
            key: self.key.clone(),
            vhost: self.vhost.clone(),
            app: self.app.clone(),
            stream: self.stream.clone(),
            dst_app: self.dst_app.clone(),
            dst_stream: self.dst_stream.clone(),
            output_codec: self.config.output_codec.clone(),
            input_codec: self.config.input_codec.clone(),
            in_frames: self.in_frames.load(Ordering::Relaxed),
            out_frames: self.out_frames.load(Ordering::Relaxed),
            created_at: self.created_at,
        }
    }

    /// Runs the transcode loop until the source closes or the session is
    /// stopped. Subscribes to the source, feeds each video frame to ffmpeg,
    /// and republishes transcoded Annex-B output.
    async fn run(
        config: TranscodeConfig,
        source: Arc<MediaSource>,
        dest: Arc<MediaSource>,
        in_frames: Arc<AtomicU64>,
        out_frames: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
    ) {
        let mut transcoder = match StreamingTranscoder::new(&config).await {
            Ok(t) => t,
            Err(e) => {
                warn!("transcode {} failed to start ffmpeg: {}", source.id, e);
                running.store(false, Ordering::Relaxed);
                return;
            }
        };

        // Replay existing GOP so the transcoder starts on a key frame, and so
        // we can emit SPS/PPS before the first frame.
        let gop = source.get_latest_gop_frames().await;
        for f in &gop {
            if f.frame_type != FrameType::Video {
                continue;
            }
            if f.config_frame {
                let cfg = config_frame_to_annex_b(&f.data);
                if !cfg.is_empty() {
                    let _ = transcoder.feed(&cfg).await;
                }
            } else {
                let annex_b = flv_video_to_annex_b(&f.data);
                if !annex_b.is_empty() {
                    let _ = transcoder.feed(&annex_b).await;
                    in_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let mut rx = source.subscribe();
        let mut buf: Vec<u8> = Vec::new();
        let mut last_ts: u32 = 0;

        loop {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            tokio::select! {
                frame = rx.recv() => {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(_) => break, // source closed
                    };
                    if frame.frame_type != FrameType::Video {
                        continue;
                    }
                    if frame.config_frame {
                        let cfg = config_frame_to_annex_b(&frame.data);
                        if !cfg.is_empty() {
                            let _ = transcoder.feed(&cfg).await;
                        }
                        continue;
                    }
                    let annex_b = flv_video_to_annex_b(&frame.data);
                    if annex_b.is_empty() {
                        continue;
                    }
                    last_ts = frame.timestamp;
                    let _ = transcoder.feed(&annex_b).await;
                    in_frames.fetch_add(1, Ordering::Relaxed);
                }
                out = transcoder.recv() => {
                    let chunk = match out {
                        Some(c) => c,
                        None => break, // ffmpeg ended
                    };
                    buf.extend_from_slice(&chunk);
                    // Emit complete access units delimited by start codes.
                    let mut pos = 0;
                    while let Some(end) = next_frame_boundary(&buf, pos) {
                        let unit = buf[pos..end].to_vec();
                        pos = end;
                        if unit.len() < 4 {
                            continue;
                        }
                        let key = is_keyframe(&unit, &config.output_codec);
                        let idx = out_frames.load(Ordering::Relaxed);
                        // 90 kHz video clock; assume ~25 fps → 3600 ticks/frame.
                        let ts = (idx * 3600) as u32;
                        let _ = last_ts;
                        let out_frame = MediaFrame::new_video(
                            0,
                            codec_from_str(&config.output_codec),
                            ts,
                            ts as u64,
                            ts as u64,
                            Bytes::from(unit),
                            key,
                        );
                        dest.publish_and_cache(out_frame).await;
                        out_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    if pos > 0 {
                        buf.drain(..pos);
                    }
                }
            }
        }

        info!("transcode session {} stopped", source.id);
    }

    /// Stops the session and closes the destination source.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.dest.close();
    }
}

impl Drop for TranscodeSession {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

/// Manages all active transcoding sessions, keyed by instance key.
#[derive(Clone)]
pub struct TranscodeManager {
    sessions: Arc<DashMap<String, Arc<tokio::sync::Mutex<TranscodeSession>>>>,
    sources: Arc<MediaSourceManager>,
}

impl TranscodeManager {
    pub fn new(sources: Arc<MediaSourceManager>) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            sources,
        }
    }

    /// Adds (or returns existing) transcoding for `vhost/app/stream`.
    ///
    /// Returns the instance info. The output stream is published to the
    /// shared `MediaSourceManager`, so it can be played back over any
    /// protocol immediately.
    pub async fn add_transcode(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
        config: TranscodeConfig,
    ) -> Result<TranscodeInfo> {
        let name = config
            .dst_stream
            .clone()
            .unwrap_or_else(|| format!("{}{}", stream, config.name));
        let dst_app = config.dst_app.clone().unwrap_or_else(|| app.to_string());
        let key = format!("{}/{}/{}/{}", vhost, app, stream, name);

        if let Some(existing) = self.sessions.get(&key) {
            return Ok(existing.lock().await.info());
        }

        let source = self.sources.get_or_create(vhost, app, stream);
        let dest = self.sources.get_or_create(vhost, &dst_app, &name);

        let in_frames = Arc::new(AtomicU64::new(0));
        let out_frames = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let cfg = config.clone();
        let src = source.clone();
        let dst = dest.clone();
        let in_c = in_frames.clone();
        let out_c = out_frames.clone();
        let run_c = running.clone();
        let handle = tokio::spawn(async move {
            TranscodeSession::run(cfg, src, dst, in_c, out_c, run_c).await;
        });

        let session = TranscodeSession {
            key: key.clone(),
            vhost: vhost.to_string(),
            app: app.to_string(),
            stream: stream.to_string(),
            dst_app: dst_app.clone(),
            dst_stream: name.clone(),
            config,
            source,
            dest,
            in_frames,
            out_frames,
            running,
            handle: Some(handle),
            created_at: chrono::Utc::now().timestamp(),
        };

        self.sessions
            .insert(key.clone(), Arc::new(tokio::sync::Mutex::new(session)));
        info!("add_transcode: {} -> {}/{}", key, dst_app, name);
        let info = self.sessions.get(&key).unwrap().lock().await.info();
        Ok(info)
    }

    /// Removes a transcode session by its key. Returns true if removed.
    pub fn del_transcode(&self, key: &str) -> bool {
        if let Some((_, session)) = self.sessions.remove(key) {
            tokio::spawn(async move {
                session.lock().await.stop();
            });
            true
        } else {
            false
        }
    }

    /// Looks up a transcode session by key.
    pub async fn get_transcode(&self, key: &str) -> Option<TranscodeInfo> {
        match self.sessions.get(key) {
            Some(s) => Some(s.lock().await.info()),
            None => None,
        }
    }

    /// Lists all active transcode sessions.
    pub async fn list_transcode(&self) -> Vec<TranscodeInfo> {
        let mut out = Vec::new();
        for entry in self.sessions.iter() {
            out.push(entry.value().lock().await.info());
        }
        out
    }
}

// ── helpers ───────────────────────────────────────────────────────────

fn codec_from_str(s: &str) -> CodecId {
    match s.to_ascii_lowercase().as_str() {
        "h264" | "avc" => CodecId::H264,
        "h265" | "hevc" => CodecId::H265,
        _ => CodecId::H264,
    }
}

/// Returns the exclusive end index of the current access unit (the position
/// of the next start code), or None if no following start code is present.
fn next_frame_boundary(buf: &[u8], pos: usize) -> Option<usize> {
    let len = buf.len();
    let mut i = pos + 1;
    while i + 2 < len {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 3 < len && buf[i + 2] == 1 {
                return Some(i);
            }
            if i + 4 <= len && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn is_keyframe(data: &[u8], codec: &str) -> bool {
    match codec.to_ascii_lowercase().as_str() {
        "h264" | "avc" => {
            for w in data.windows(5) {
                let sc = (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1);
                if sc {
                    let t = if w[2] == 1 { w[3] & 0x1F } else { w[4] & 0x1F };
                    if t == 5 {
                        return true;
                    }
                }
            }
            false
        }
        "h265" | "hevc" => {
            for w in data.windows(4) {
                let sc = (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1);
                if sc {
                    let b = w[3];
                    let t = (b >> 1) & 0x3F;
                    if (16..=21).contains(&t) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zlmediakit_core::MediaSourceManager;

    #[test]
    fn test_transcode_config_from_api_params() {
        let cfg = TranscodeConfig::from_api_params(
            "h264",
            "H265",
            Some((640, 360)),
            Some("1000"),
            Some("_trans"),
            Some("dstapp"),
            Some("dststream"),
            None,
        );
        assert_eq!(cfg.input_codec, "h264");
        assert_eq!(cfg.output_codec, "h265");
        assert_eq!(cfg.width, Some(640));
        assert_eq!(cfg.height, Some(360));
        assert_eq!(cfg.bitrate, "1000");
        assert_eq!(cfg.name, "_trans");
        assert_eq!(cfg.dst_app.as_deref(), Some("dstapp"));
        assert_eq!(cfg.dst_stream.as_deref(), Some("dststream"));
    }

    #[tokio::test]
    async fn test_transcode_manager_list_and_del() {
        let mgr = TranscodeManager::new(Arc::new(MediaSourceManager::new()));
        assert!(mgr.list_transcode().await.is_empty());

        let cfg = TranscodeConfig::from_api_params(
            "h264",
            "h264",
            None,
            Some("1M"),
            Some("_out"),
            None,
            Some("out1"),
            None,
        );
        // ffmpeg may not be installed; the manager still registers the session
        // (the inner ffmpeg failure is logged, not fatal to registration).
        let _ = mgr
            .add_transcode("__defaultVhost__", "live", "src", cfg)
            .await;

        let list = mgr.list_transcode().await;
        assert_eq!(list.len(), 1);
        let key = &list[0].key;
        assert!(mgr.del_transcode(key));
        assert!(!mgr.del_transcode("nonexistent/key"));
    }

    #[test]
    fn test_is_keyframe_helpers() {
        // H264 IDR (type 5) NAL after a 4-byte start code.
        let h264_idr = vec![0, 0, 0, 1, 0x65, 0x01, 0x02];
        assert!(is_keyframe(&h264_idr, "h264"));
        let h264_non_idr = vec![0, 0, 1, 0x41, 0x01];
        assert!(!is_keyframe(&h264_non_idr, "h264"));
    }
}
