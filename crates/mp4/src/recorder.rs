use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};
use zlmediakit_core::media_source::MediaSource;

use crate::muxer::Mp4Muxer;

/// Records a published `MediaSource` to a local MP4 file.
///
/// Unlike FLV (which can be written incrementally) the recorder buffers every
/// frame in memory so that it can write the ISO base media file in the correct
/// box order with proper sample tables.  This is fine for typical recording
/// lengths; for very long sessions the memory footprint should be monitored.
pub struct Mp4Recorder;

impl Mp4Recorder {
    pub async fn record(
        source: Arc<MediaSource>,
        base_dir: &str,
        stop: Arc<Notify>,
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
            info!("MP4 record: no frames received, skipping {}", path.display());
            return Ok(());
        }

        let file = tokio::fs::File::create(&path).await?;
        let mut file = tokio::io::BufWriter::new(file);
        file.write_all(&output).await?;
        file.flush().await?;

        info!("MP4 record: finished {} ({} bytes)", path.display(), output.len());
        Ok(())
    }
}
