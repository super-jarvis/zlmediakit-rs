use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};
use zlmediakit_core::event_bus::{Event, EventBus};
use zlmediakit_core::media_source::MediaSource;

use crate::muxer::Mp4Muxer;

/// Records a published `MediaSource` to a local MP4 file.
///
/// Unlike FLV (which can be written incrementally) the recorder buffers every
/// frame in memory so that it can write the ISO base media file in the correct
/// box order with proper sample tables.  This is fine for typical recording
/// lengths; for very long sessions the memory footprint should be monitored.
///
/// When `event_bus` is provided, a `RecordMp4Done` event is published once the
/// file has been written, so external hooks (`on_record_mp4`) can be fired.
pub struct Mp4Recorder;

impl Mp4Recorder {
    pub async fn record(
        source: Arc<MediaSource>,
        base_dir: &str,
        stop: Arc<Notify>,
        event_bus: Option<Arc<EventBus>>,
    ) -> anyhow::Result<()> {
        let dir = PathBuf::from(base_dir)
            .join(&source.app)
            .join(&source.stream);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            error!("MP4 record: failed to create dir {}: {}", dir.display(), e);
            return Err(e.into());
        }

        let file_name = format!("{}.mp4", Utc::now().format("%Y%m%d_%H%M%S"));
        let path = dir.join(&file_name);
        info!("MP4 record: starting {}", path.display());

        let mut muxer = Mp4Muxer::new();

        let config_frames = source.get_cached_config_frames().await;
        for frame in &config_frames {
            muxer.push_frame(frame);
        }

        // A recorder can be started while the publisher is already active.
        // Replay the latest cached GOP before subscribing so frames published
        // between task creation and receiver registration are not lost.
        let cached_gop = source.get_latest_gop_frames().await;
        for frame in &cached_gop {
            if !zlmediakit_core::gop_cache::is_config_frame(frame) {
                muxer.push_frame(frame);
            }
        }

        let mut min_dts: Option<u64> = None;
        let mut max_dts: Option<u64> = None;

        let mut rx = source.subscribe();
        loop {
            tokio::select! {
                _ = stop.notified() => {
                    info!("MP4 record: stop signal for {}", path.display());
                    break;
                }
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            if let Some(d) = frame.dts.checked_mul(1) {
                                min_dts = Some(min_dts.map_or(d, |m| m.min(d)));
                                max_dts = Some(max_dts.map_or(d, |m| m.max(d)));
                            }
                            muxer.push_frame(&frame);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("MP4 record: lagged by {} frames, continuing", n);
                        }
                        Err(_) => {
                            info!("MP4 record: source closed for {}", path.display());
                            break;
                        }
                    }
                }
            }
        }

        let output = muxer.finalize();
        if output.is_empty() {
            info!(
                "MP4 record: no frames received, skipping {}",
                path.display()
            );
            return Ok(());
        }

        let file = tokio::fs::File::create(&path).await?;
        let mut file = tokio::io::BufWriter::new(file);
        file.write_all(&output).await?;
        file.flush().await?;

        let file_size = output.len() as u64;
        // dts is in milliseconds in this codebase; convert to seconds.
        let time_len = match (min_dts, max_dts) {
            (Some(min), Some(max)) if max > min => (max - min) as f64 / 1000.0,
            _ => 0.0,
        };

        info!(
            "MP4 record: finished {} ({} bytes, {:.2}s)",
            path.display(),
            file_size,
            time_len
        );

        if let Some(eb) = &event_bus {
            eb.publish(Event::RecordMp4Done {
                vhost: source.vhost.clone(),
                app: source.app.clone(),
                stream: source.stream.clone(),
                file_path: path.to_string_lossy().to_string(),
                file_size,
                time_len,
            });
        }

        Ok(())
    }
}
