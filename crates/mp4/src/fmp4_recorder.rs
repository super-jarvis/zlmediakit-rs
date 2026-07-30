//! fMP4 (fragmented MP4) recorder for live streaming.
//!
//! Unlike `Mp4Recorder` which writes a complete MP4 at stream end,
//! this recorder writes fMP4 init+media segments continuously.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{error, info};
use zlmediakit_core::gop_cache;
use zlmediakit_core::media_source::MediaSource;

use crate::fmp4::Fmp4Muxer;

pub struct Fmp4Recorder;

impl Fmp4Recorder {
    /// Records a live stream as fMP4 segments.
    ///
    /// Writes `init.mp4` and `seg-{index}.m4s` files to `base_dir`.
    pub async fn record(
        source: Arc<MediaSource>,
        base_dir: &str,
        stop: Arc<Notify>,
    ) -> anyhow::Result<()> {
        let dir = PathBuf::from(base_dir)
            .join(&source.app)
            .join(&source.stream);
        tokio::fs::create_dir_all(&dir).await?;
        info!("fMP4 recorder started -> {}", dir.display());

        let mut muxer = Fmp4Muxer::new(90000);
        let mut rx = source.subscribe();

        // Replay config
        let configs = {
            let cache = source.gop_cache.read().await;
            cache.get_config_frames()
        };
        for f in &configs {
            muxer.push_frame(f);
        }

        // Write init segment
        let init = muxer.init_segment();
        let init_path = dir.join("init.mp4");
        tokio::fs::write(&init_path, &init.data).await?;
        info!("  wrote init segment: {}", init_path.display());

        // Replay GOP
        let gop = {
            let cache = source.gop_cache.read().await;
            cache.get_latest_gop_frames()
        };
        for f in &gop {
            if !gop_cache::is_config_frame(f) {
                muxer.push_frame(f);
            }
        }

        let mut seg_index: u32 = 0;
        let flush_dur = std::time::Duration::from_secs(4);

        loop {
            tokio::select! {
                frame = rx.recv() => {
                    match frame {
                        Ok(f) => {
                            if !gop_cache::is_config_frame(&f) {
                                muxer.push_frame(&f);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep(flush_dur) => {
                    if let Some(seg) = muxer.flush_segment() {
                        let seg_path = dir.join(format!("seg-{}.m4s", seg_index));
                        if let Err(e) = tokio::fs::write(&seg_path, &seg.data).await {
                            error!("fMP4 recorder write error: {}", e);
                            break;
                        }
                        info!("  wrote segment {} ({} bytes)", seg_index, seg.data.len());
                        seg_index += 1;
                    }
                }
                _ = stop.notified() => {
                    // Flush remaining and exit
                    if let Some(seg) = muxer.flush_segment() {
                        let seg_path = dir.join(format!("seg-{}.m4s", seg_index));
                        let _ = tokio::fs::write(&seg_path, &seg.data).await;
                    }
                    break;
                }
            }
        }

        info!("fMP4 recorder stopped: {} segments written", seg_index);
        Ok(())
    }
}
