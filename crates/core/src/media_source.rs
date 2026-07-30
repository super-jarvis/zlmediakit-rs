use crate::gop_cache::GopCache;
use crate::media_frame::{MediaFrame, MediaInfo};
use crate::session::SessionId;
use crate::Result;
#[cfg(test)]
use bytes::Bytes;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_broadcast_delivery() {
        let source = Arc::new(MediaSource::new(
            "test".into(),
            "vhost".into(),
            "app".into(),
            "stream".into(),
        ));
        let source_clone = source.clone();

        // Publisher sends frames
        tokio::spawn(async move {
            for i in 0..100 {
                let is_key = i == 0 || i % 10 == 0;
                let frame = MediaFrame::new_video(
                    0,
                    crate::media_frame::CodecId::H264,
                    i as u32,
                    i as u64,
                    i as u64,
                    Bytes::from(vec![0x17, if is_key { 1 } else { 1 }, 0x00, 0x00, 0x00]),
                    is_key,
                );
                source_clone.publish_and_cache(frame).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        // Late subscriber (after 200ms = ~20 frames)
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut rx = source.subscribe();
        let timeout = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(timeout);

        let mut received = 0u32;
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            received += 1;
                            eprintln!("RECV: frame {} key={}", received, frame.key_frame);
                            if received >= 5 {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("Lagged by {}", n);
                        }
                        Err(_) => break,
                    }
                }
                _ = &mut timeout => {
                    eprintln!("TIMEOUT: only received {} frames", received);
                    panic!("Timeout waiting for frames, got only {}", received);
                }
            }
        }

        assert!(
            received >= 5,
            "Should have received at least 5 frames, got {}",
            received
        );
    }
}

pub type SourceId = String;

#[derive(Debug, Clone)]
pub struct MediaSource {
    pub id: SourceId,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub info: Arc<RwLock<MediaInfo>>,
    pub frame_tx: Arc<Mutex<Option<broadcast::Sender<MediaFrame>>>>,
    pub gop_cache: Arc<RwLock<GopCache>>,
    pub subscribers: Arc<RwLock<HashMap<SessionId, ()>>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl MediaSource {
    pub fn new(id: SourceId, vhost: String, app: String, stream: String) -> Self {
        let (frame_tx, _) = broadcast::channel(256);
        Self {
            id,
            vhost,
            app,
            stream,
            info: Arc::new(RwLock::new(MediaInfo {
                tracks: vec![],
                duration: None,
                metadata: None,
            })),
            frame_tx: Arc::new(Mutex::new(Some(frame_tx))),
            gop_cache: Arc::new(RwLock::new(GopCache::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            created_at: chrono::Utc::now(),
        }
    }

    pub fn url(&self) -> String {
        format!("{}/{}", self.app, self.stream)
    }

    pub async fn update_info(&self, info: MediaInfo) {
        *self.info.write().await = info;
    }

    pub async fn update_tracks(&self, tracks: Vec<crate::media_frame::TrackInfo>) {
        let mut info = self.info.write().await;
        info.tracks = tracks;
    }

    pub async fn add_subscriber(&self, session_id: SessionId) {
        self.subscribers.write().await.insert(session_id, ());
    }

    pub async fn remove_subscriber(&self, session_id: &SessionId) {
        self.subscribers.write().await.remove(session_id);
    }

    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }

    fn lock_frame_tx(&self) -> std::sync::MutexGuard<'_, Option<broadcast::Sender<MediaFrame>>> {
        // Recover from poisoned mutex — the inner data is still valid.
        self.frame_tx.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MediaFrame> {
        let guard = self.lock_frame_tx();
        match guard.as_ref() {
            Some(tx) => tx.subscribe(),
            None => {
                let (_, rx) = broadcast::channel(1);
                rx
            }
        }
    }

    /// Closes the broadcast channel by dropping the sender, so all current
    /// subscribers (players) receive `RecvError::Closed` and terminate. Used by
    /// the management API to kick a stream. New players fail because the source
    /// is also removed from the manager.
    pub fn close(&self) {
        let _ = self.lock_frame_tx().take();
    }

    pub fn publish(&self, frame: MediaFrame) -> Result<()> {
        if let Some(tx) = self.lock_frame_tx().as_ref() {
            let _ = tx.send(frame.clone());
        }
        Ok(())
    }

    pub async fn publish_and_cache(&self, frame: MediaFrame) {
        {
            let mut cache = self.gop_cache.write().await;
            cache.cache_frame(&frame);
        }
        let (receivers, result) = {
            let guard = self.lock_frame_tx();
            match guard.as_ref() {
                Some(tx) => (tx.receiver_count(), tx.send(frame.clone())),
                None => (0, Ok(0)),
            }
        };
        debug!(
            "publish_and_cache: type={:?} key={} receivers={} send_result={:?}",
            frame.frame_type,
            frame.key_frame,
            receivers,
            result.is_ok()
        );
    }

    pub async fn get_cached_config_frames(&self) -> Vec<MediaFrame> {
        let cache = self.gop_cache.read().await;
        cache.get_config_frames()
    }

    /// Returns the cached metadata/config frames plus the most recent GOP, so a
    /// late subscriber (or recorder) can begin decoding from a key frame.
    pub async fn get_latest_gop_frames(&self) -> Vec<MediaFrame> {
        let cache = self.gop_cache.read().await;
        cache.get_latest_gop_frames()
    }
}

pub struct MediaSourceManager {
    sources: DashMap<SourceId, Arc<MediaSource>>,
}

impl MediaSourceManager {
    pub fn new() -> Self {
        Self {
            sources: DashMap::new(),
        }
    }

    pub fn get_or_create(&self, vhost: &str, app: &str, stream: &str) -> Arc<MediaSource> {
        let id = format!("{}/{}/{}", vhost, app, stream);
        self.sources
            .entry(id.clone())
            .or_insert_with(|| {
                info!("Creating media source: {}", id);
                Arc::new(MediaSource::new(
                    id,
                    vhost.to_string(),
                    app.to_string(),
                    stream.to_string(),
                ))
            })
            .clone()
    }

    pub fn get(&self, vhost: &str, app: &str, stream: &str) -> Option<Arc<MediaSource>> {
        let id = format!("{}/{}/{}", vhost, app, stream);
        self.sources.get(&id).map(|s| s.clone())
    }

    pub fn remove(&self, vhost: &str, app: &str, stream: &str) -> Option<Arc<MediaSource>> {
        let id = format!("{}/{}/{}", vhost, app, stream);
        self.sources.remove(&id).map(|(_, s)| s)
    }

    /// Closes a stream (kicks all viewers and removes it from the registry) and
    /// returns whether a stream was actually found and closed.
    pub fn close_stream(&self, vhost: &str, app: &str, stream: &str) -> bool {
        let id = format!("{}/{}/{}", vhost, app, stream);
        if let Some((_, source)) = self.sources.remove(&id) {
            source.close();
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<Arc<MediaSource>> {
        self.sources.iter().map(|s| s.clone()).collect()
    }

    pub fn count(&self) -> usize {
        self.sources.len()
    }
}

impl Default for MediaSourceManager {
    fn default() -> Self {
        Self::new()
    }
}
