use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};
use zlmediakit_core::media_source::MediaSource;

use crate::muxer::FlvMuxer;

/// Records a published `MediaSource` to a local FLV file.
///
/// The recorder first replays the cached config + latest GOP so the resulting
/// file is independently decodable, then writes every live frame as it arrives.
/// Recording stops when the shared `Notify` is triggered (stream unpublish).
pub struct FlvRecorder;

impl FlvRecorder {
    pub async fn record(
        source: Arc<MediaSource>,
        base_dir: &str,
        stop: Arc<Notify>,
    ) -> anyhow::Result<()> {
        let dir = PathBuf::from(base_dir)
            .join(&source.app)
            .join(&source.stream);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            error!("FLV record: failed to create dir {}: {}", dir.display(), e);
            return Err(e.into());
        }

        let file_name = format!("{}.flv", Utc::now().format("%Y%m%d_%H%M%S"));
        let path = dir.join(&file_name);
        info!("FLV record: starting {}", path.display());

        let file = tokio::fs::File::create(&path).await?;
        let mut file = tokio::io::BufWriter::new(file);

        let mut muxer = FlvMuxer::new();
        file.write_all(&muxer.write_header()).await?;

        // Subscribe first so we never miss a frame published after this point,
        // then replay the historical config + GOP from the cache.
        let mut rx = source.subscribe();
        let gop = source.get_latest_gop_frames().await;
        for frame in &gop {
            file.write_all(&muxer.write_tag(frame)?).await?;
        }

        let mut since_flush: usize = 0;
        loop {
            tokio::select! {
                _ = stop.notified() => {
                    info!("FLV record: stop signal for {}", path.display());
                    break;
                }
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            file.write_all(&muxer.write_tag(&frame)?).await?;
                            since_flush += 1;
                            if since_flush >= 50 {
                                file.flush().await?;
                                since_flush = 0;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("FLV record: lagged by {} frames, continuing", n);
                        }
                        Err(_) => {
                            info!("FLV record: source closed for {}", path.display());
                            break;
                        }
                    }
                }
            }
        }

        file.flush().await?;
        info!("FLV record: finished {}", path.display());
        Ok(())
    }
}
