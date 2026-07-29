use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};
use zlmediakit_core::media_source::MediaSource;

use crate::muxer::HlsMuxer;

/// Records a published `MediaSource` to a local HLS VOD archive (`.ts` segments
/// plus a `.m3u8` playlist). The playlist is rewritten as each segment is
/// produced; recording stops when the shared `Notify` is triggered.
pub struct HlsRecorder;

impl HlsRecorder {
    pub async fn record(
        source: Arc<MediaSource>,
        base_dir: &str,
        stop: Arc<Notify>,
    ) -> anyhow::Result<()> {
        let dir = PathBuf::from(base_dir)
            .join(&source.app)
            .join(&source.stream);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            error!("HLS record: failed to create dir {}: {}", dir.display(), e);
            return Err(e.into());
        }
        info!("HLS record: starting in {}", dir.display());

        let mut muxer = HlsMuxer::new(2.0);

        // Replay cached config frames so the first segment can be muxed.
        let config_frames = source.get_cached_config_frames().await;
        for frame in &config_frames {
            let _ = muxer.push_frame(frame.clone());
        }

        let mut rx = source.subscribe();
        let mut playlist: Vec<(u32, f64)> = Vec::new();

        loop {
            tokio::select! {
                _ = stop.notified() => {
                    info!("HLS record: stop signal for {}", dir.display());
                    break;
                }
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            if let Some((idx, data)) = muxer.push_frame(frame) {
                                let fname = format!("{}.ts", idx);
                                if let Err(e) = tokio::fs::write(dir.join(&fname), &data).await {
                                    error!("HLS record: write segment {} failed: {}", fname, e);
                                } else {
                                    playlist.push((idx, 2.0));
                                    Self::write_playlist(&dir, &playlist).await;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("HLS record: lagged by {} frames, continuing", n);
                        }
                        Err(_) => {
                            info!("HLS record: source closed for {}", dir.display());
                            break;
                        }
                    }
                }
            }
        }

        // Flush any remaining buffered frames into a final segment.
        if let Some((idx, data)) = muxer.flush() {
            let fname = format!("{}.ts", idx);
            if tokio::fs::write(dir.join(&fname), &data).await.is_ok() {
                playlist.push((idx, 2.0));
            }
        }
        Self::write_playlist(&dir, &playlist).await;
        info!("HLS record: finished in {}", dir.display());
        Ok(())
    }

    async fn write_playlist(dir: &Path, playlist: &[(u32, f64)]) {
        let mut content = String::from(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n",
        );
        for (idx, dur) in playlist {
            content.push_str(&format!("#EXTINF:{:.3},\n{}.ts\n", dur, idx));
        }
        content.push_str("#EXT-X-ENDLIST\n");
        let _ = tokio::fs::write(dir.join("record.m3u8"), content).await;
    }
}
