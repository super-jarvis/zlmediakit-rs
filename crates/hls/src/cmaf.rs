//! CMAF-compatible HLS with fMP4 segments.
//!
//! Uses `Fmp4Muxer` to generate init segments and media segments,
//! storing them in a shared cache. Generates m3u8 playlists with
//! `#EXT-X-MAP` tags instead of TS-based HLS.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use zlmediakit_core::media_source::MediaSource;
use zlmediakit_mp4::fmp4::Fmp4Muxer;

use crate::muxer::HlsSegment;


/// Starts a CMAF HLS task for a live stream.
///
/// Subscribes to the MediaSource, generates fMP4 segments every 4 seconds,
/// and stores them (plus the init segment) in the provided caches.
pub fn start_hls_cmaf_task(
    source: Arc<MediaSource>,
    segments: Arc<RwLock<VecDeque<HlsSegment>>>,
    init_cache: Arc<RwLock<Option<Vec<u8>>>>,
) {
    tokio::spawn(async move {
        let mut muxer = Fmp4Muxer::new(90000);
        let mut rx = source.subscribe();

        // Replay config frames
        let configs = {
            let cache = source.gop_cache.read().await;
            cache.get_config_frames()
        };
        for f in &configs {
            muxer.push_frame(f);
        }

        // Store init segment
        let init = muxer.init_segment();
        *init_cache.write().await = Some(init.data.to_vec());

        // Replay GOP cache
        let gop = {
            let cache = source.gop_cache.read().await;
            cache.get_latest_gop_frames()
        };
        for f in &gop {
            if !zlmediakit_core::gop_cache::is_config_frame(f) {
                muxer.push_frame(f);
            }
        }

        let mut seg_index: u32 = 0;
        let mut last_flush = tokio::time::Instant::now();
        let flush_interval = tokio::time::Duration::from_secs(4);
        let mut seg_start = last_flush;

        loop {
            tokio::select! {
                frame = rx.recv() => {
                    match frame {
                        Ok(f) => {
                            if !zlmediakit_core::gop_cache::is_config_frame(&f) {
                                muxer.push_frame(&f);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(last_flush + flush_interval) => {
                    if let Some(seg) = muxer.flush_segment() {
                        let now = tokio::time::Instant::now();
                        let dur = now.duration_since(seg_start).as_secs_f64();
                        seg_start = now;

                        let mut guard = segments.write().await;
                        guard.push_back(HlsSegment {
                            index: seg_index,
                            duration: dur.max(1.0),
                            data: seg.data.to_vec(),
                        });
                        seg_index += 1;
                        while guard.len() > 10 {
                            guard.pop_front();
                        }
                    }
                    last_flush = tokio::time::Instant::now();
                }
            }
        }
    });
}

/// Generates a CMAF-compatible m3u8 playlist.
///
/// `segments` are the cached media segments, `target_duration` is the
/// maximum segment duration, `app`/`stream` are used to build segment URLs.
pub fn build_cmaf_playlist(
    segments: &VecDeque<HlsSegment>,
    target_duration: u32,
    app: &str,
    stream: &str,
) -> String {
    let mut m3u8 = String::new();
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration.max(1)));
    m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");

    if !segments.is_empty() {
        let first_idx = segments.front().map(|s| s.index).unwrap_or(0);
        m3u8.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", first_idx));
        // CMAF init segment reference
        m3u8.push_str(&format!(
            "#EXT-X-MAP:URI=\"/{}/{}/init.mp4\"\n",
            app, stream
        ));
        for seg in segments.iter() {
            m3u8.push_str(&format!("#EXTINF:{:.3},\n", seg.duration));
            m3u8.push_str(&format!(
                "/{}/{}/seg-{}.m4s\n",
                app, stream, seg.index
            ));
        }
    } else {
        m3u8.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    }

    m3u8
}

// Import needed for the select! macro usage
