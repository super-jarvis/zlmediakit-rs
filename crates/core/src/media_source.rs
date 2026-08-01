use crate::event_bus::{Event, EventBus};
use crate::gop_cache::GopCache;
use crate::media_frame::{MediaFrame, MediaInfo};
use crate::session::SessionId;
use crate::stream_proxy::StreamProxyControl;
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
            None,
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
                    Bytes::from(vec![0x17, 1, 0x00, 0x00, 0x00]),
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
    pub event_bus: Option<Arc<EventBus>>,
    /// Per-source traffic counters (bytes). Shared so the manager can read them
    /// when summarizing total traffic for the `on_flow_report` hook.
    pub bytes_in: Arc<std::sync::atomic::AtomicU64>,
    pub bytes_out: Arc<std::sync::atomic::AtomicU64>,
}

impl MediaSource {
    pub fn new(
        id: SourceId,
        vhost: String,
        app: String,
        stream: String,
        event_bus: Option<Arc<EventBus>>,
    ) -> Self {
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
            event_bus,
            bytes_in: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bytes_out: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        let was_empty = self.subscribers.read().await.is_empty();
        self.subscribers
            .write()
            .await
            .insert(session_id.clone(), ());
        if was_empty {
            // First subscriber → stream changed (regist=true).
            if let Some(eb) = &self.event_bus {
                eb.publish(Event::StreamPlay {
                    source_id: self.id.clone(),
                    session_id: session_id.clone(),
                });
            }
        }
    }

    pub async fn remove_subscriber(&self, session_id: &SessionId) {
        self.subscribers.write().await.remove(session_id);
        let count = self.subscribers.read().await.len();
        if let Some(eb) = &self.event_bus {
            eb.publish(Event::StreamStop {
                source_id: self.id.clone(),
                session_id: session_id.clone(),
            });
            if count == 0 {
                eb.publish(Event::NoReader {
                    source_id: self.id.clone(),
                });
            }
        }
    }

    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }

    /// Snapshot of the session ids currently playing this source.
    pub async fn subscriber_ids(&self) -> Vec<SessionId> {
        self.subscribers.read().await.keys().cloned().collect()
    }

    /// Non-async variant of [`subscriber_count`], safe to call from a synchronous
    /// context (e.g. an event-bus handler) since the inner lock is an
    /// `RwLock` with a non-blocking read path.
    pub fn subscriber_count_blocking(&self) -> usize {
        self.subscribers
            .try_read()
            .map(|g| g.len())
            .unwrap_or_else(|_| self.subscribers.blocking_read().len())
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
        let payload_len = frame.data.len() as u64;
        self.bytes_in.fetch_add(payload_len, std::sync::atomic::Ordering::Relaxed);
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
        // Traffic delivered to players (one copy per live subscriber).
        if receivers > 0 {
            self.bytes_out
                .fetch_add(payload_len * receivers as u64, std::sync::atomic::Ordering::Relaxed);
        }
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
    event_bus: Option<Arc<EventBus>>,
    /// Stream proxy control used for edge auto-pull. When set together with an
    /// origin URL (configured at runtime via `set_origin`), a play request for
    /// a missing stream triggers an automatic pull from the origin server.
    proxy: std::sync::RwLock<Option<Arc<StreamProxyControl>>>,
    /// Origin server base URL for edge auto-pull (e.g. `rtmp://origin:1935`).
    origin: std::sync::RwLock<Option<String>>,
    /// Local vhost used when pulling from the origin.
    origin_vhost: std::sync::RwLock<String>,
    /// Local app used when pulling from the origin.
    origin_app: std::sync::RwLock<String>,
}

impl MediaSourceManager {
    pub fn new(event_bus: Option<Arc<EventBus>>) -> Self {
        Self {
            sources: DashMap::new(),
            event_bus,
            proxy: std::sync::RwLock::new(None),
            origin: std::sync::RwLock::new(None),
            origin_vhost: std::sync::RwLock::new("__defaultVhost__".to_string()),
            origin_app: std::sync::RwLock::new("live".to_string()),
        }
    }

    /// Enables edge auto-pull: play requests for unknown streams will be
    /// satisfied by pulling them from `origin_url` (e.g. `rtmp://origin:1935`).
    pub fn set_origin_pull(
        &self,
        proxy: Arc<StreamProxyControl>,
        origin_url: String,
        vhost: String,
        app: String,
    ) {
        *self.proxy.write().unwrap() = Some(proxy);
        *self.origin.write().unwrap() = Some(origin_url);
        *self.origin_vhost.write().unwrap() = vhost;
        *self.origin_app.write().unwrap() = app;
    }

    pub fn get_or_create(&self, vhost: &str, app: &str, stream: &str) -> Arc<MediaSource> {
        let id = format!("{}/{}/{}", vhost, app, stream);
        let eb = self.event_bus.clone();
        self.sources
            .entry(id.clone())
            .or_insert_with(|| {
                info!("Creating media source: {}", id);
                Arc::new(MediaSource::new(
                    id,
                    vhost.to_string(),
                    app.to_string(),
                    stream.to_string(),
                    eb,
                ))
            })
            .clone()
    }

    /// Summarizes total bytes in/out across all live sources. Used for the
    /// `on_flow_report` hook.
    pub fn total_traffic(&self) -> (u64, u64) {
        let mut in_sum = 0u64;
        let mut out_sum = 0u64;
        for src in self.sources.iter() {
            in_sum += src.value().bytes_in.load(std::sync::atomic::Ordering::Relaxed);
            out_sum += src.value().bytes_out.load(std::sync::atomic::Ordering::Relaxed);
        }
        (in_sum, out_sum)
    }

    /// Number of currently live (published) sources.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Like [`get`](Self::get) but, when origin-pull is configured and the
    /// stream is not currently available locally, automatically starts pulling
    /// it from the origin server and waits (up to `timeout`) for it to appear.
    ///
    /// This is the entry point used by player sessions so that an edge node can
    /// transparently serve streams that only exist on the origin.
    pub async fn get_or_pull(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
    ) -> Option<Arc<MediaSource>> {
        if let Some(s) = self.get(vhost, app, stream) {
            return Some(s);
        }
        let (proxy, origin, vh, ap) = {
            let guard = self.proxy.read().unwrap();
            let p = match guard.as_ref() {
                Some(p) => p.clone(),
                None => return None,
            };
            let o = self.origin.read().unwrap().clone();
            let vh = self.origin_vhost.read().unwrap().clone();
            let ap = self.origin_app.read().unwrap().clone();
            (p, o, vh, ap)
        };
        let origin = match origin {
            Some(o) => o,
            None => return None,
        };
        // Already pulling? don't start a second pull.
        if proxy.is_active(vhost, app, stream) {
            return self
                .wait_for_source(vhost, app, stream, std::time::Duration::from_secs(5))
                .await;
        }
        // Build the origin URL: <origin_url>/<origin_app>/<stream>
        let origin_stream = stream;
        let url = format!("{}/{}/{}", origin.trim_end_matches('/'), ap, origin_stream);
        info!(
            "edge auto-pull: {} -> {}",
            url,
            format!("{}/{}/{}", vhost, app, stream)
        );
        if !proxy.add(&url, &vh, app, stream) {
            return None;
        }
        self.wait_for_source(vhost, app, stream, std::time::Duration::from_secs(5))
            .await
    }

    async fn wait_for_source(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
        timeout: std::time::Duration,
    ) -> Option<Arc<MediaSource>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(s) = self.get(vhost, app, stream) {
                // Wait until at least one frame is cached so the player can start.
                if !s.get_latest_gop_frames().await.is_empty() {
                    return Some(s);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return self.get(vhost, app, stream);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
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

    /// Returns streams whose fields match the given (optional) filters. An
    /// `Option` filter is only applied when `Some`; empty/`__defaultVhost__`
    /// special-casing mirrors the API defaulting.
    pub fn list_filtered(
        &self,
        vhost: Option<&str>,
        app: Option<&str>,
        stream: Option<&str>,
    ) -> Vec<Arc<MediaSource>> {
        self.sources
            .iter()
            .map(|s| s.clone())
            .filter(|s| {
                let vh_ok = vhost.is_none_or(|v| s.vhost == v || v == "__defaultVhost__");
                let app_ok = app.is_none_or(|a| s.app == a);
                let st_ok = stream.is_none_or(|st| s.stream == st);
                vh_ok && app_ok && st_ok
            })
            .collect()
    }

    pub fn count(&self) -> usize {
        self.sources.len()
    }
}

impl Default for MediaSourceManager {
    fn default() -> Self {
        Self::new(None)
    }
}
