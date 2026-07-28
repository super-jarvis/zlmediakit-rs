use crate::media_frame::{MediaFrame, MediaInfo};
use crate::gop_cache::GopCache;
use crate::session::SessionId;
use crate::Result;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

pub type SourceId = String;

#[derive(Debug, Clone)]
pub struct MediaSource {
    pub id: SourceId,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub info: Arc<RwLock<MediaInfo>>,
    pub frame_tx: broadcast::Sender<MediaFrame>,
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
            frame_tx,
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

    pub fn subscribe(&self) -> broadcast::Receiver<MediaFrame> {
        self.frame_tx.subscribe()
    }

    pub fn publish(&self, frame: MediaFrame) -> Result<()> {
        let _ = self.frame_tx.send(frame.clone());
        Ok(())
    }

    pub async fn publish_and_cache(&self, frame: MediaFrame) {
        {
            let mut cache = self.gop_cache.write().await;
            cache.cache_frame(&frame);
        }
        let _ = self.frame_tx.send(frame);
    }

    pub async fn get_cached_config_frames(&self) -> Vec<MediaFrame> {
        let cache = self.gop_cache.read().await;
        cache.get_config_frames()
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
                Arc::new(MediaSource::new(id, vhost.to_string(), app.to_string(), stream.to_string()))
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
