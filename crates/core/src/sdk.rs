//! Stable, embeddable Rust API for the in-process media graph.
//!
//! Protocol servers use the same [`MediaSourceManager`] internally, so frames
//! published through this facade are immediately available to RTMP, RTSP,
//! HTTP/HLS, SRT and WebRTC outputs wired to the returned manager.

use crate::{Event, EventBus, MediaFrame, MediaInfo, MediaSource, MediaSourceManager, TrackInfo};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;

/// An in-process media engine suitable for embedding in another Rust service.
#[derive(Clone)]
pub struct EmbeddedMediaKit {
    manager: Arc<MediaSourceManager>,
    events: Arc<EventBus>,
}

impl EmbeddedMediaKit {
    /// Creates an isolated media graph and event bus.
    pub fn new(event_capacity: usize) -> Self {
        let events = Arc::new(EventBus::new(event_capacity.max(1)));
        let manager = Arc::new(MediaSourceManager::new(Some(events.clone())));
        Self { manager, events }
    }

    /// Returns the manager to pass to protocol listeners hosted by the caller.
    pub fn media_source_manager(&self) -> Arc<MediaSourceManager> {
        self.manager.clone()
    }

    /// Subscribes to media lifecycle and track-change events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Opens (or reuses) a source for publishing frames from the host process.
    pub fn publisher(&self, vhost: &str, app: &str, stream: &str) -> EmbeddedPublisher {
        EmbeddedPublisher {
            manager: self.manager.clone(),
            source: self.manager.get_or_create(vhost, app, stream),
        }
    }

    /// Subscribes to a currently registered stream.
    ///
    /// Cached configuration and GOP frames are delivered before new live
    /// frames, matching the startup behaviour expected by protocol players.
    pub async fn subscribe(
        &self,
        vhost: &str,
        app: &str,
        stream: &str,
    ) -> Option<EmbeddedSubscription> {
        let source = self.manager.get(vhost, app, stream)?;
        let receiver = source.subscribe();
        let cached = source.get_latest_gop_frames().await.into();
        Some(EmbeddedSubscription {
            source,
            cached,
            receiver,
        })
    }

    /// Returns an immutable point-in-time view of every registered source.
    pub async fn list_sources(&self) -> Vec<SourceSnapshot> {
        let mut snapshots = Vec::new();
        for source in self.manager.list() {
            snapshots.push(SourceSnapshot::from_source(&source).await);
        }
        snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        snapshots
    }

    /// Closes a stream, wakes its subscribers and removes it from the graph.
    pub fn close_stream(&self, vhost: &str, app: &str, stream: &str) -> bool {
        self.manager.close_stream(vhost, app, stream)
    }
}

impl Default for EmbeddedMediaKit {
    fn default() -> Self {
        Self::new(256)
    }
}

/// Handle used by an embedded host to publish tracks and encoded frames.
pub struct EmbeddedPublisher {
    manager: Arc<MediaSourceManager>,
    source: Arc<MediaSource>,
}

impl EmbeddedPublisher {
    pub fn source_id(&self) -> &str {
        &self.source.id
    }

    pub async fn set_tracks(&self, tracks: Vec<TrackInfo>) {
        self.source.update_tracks(tracks).await;
    }

    pub async fn publish(&self, frame: MediaFrame) {
        self.source.publish_and_cache(frame).await;
    }

    pub fn source(&self) -> Arc<MediaSource> {
        self.source.clone()
    }

    /// Explicitly ends this publication. Dropping a handle alone does not
    /// close a source because protocol publishers may share the same graph.
    pub fn unpublish(self) -> bool {
        self.manager
            .close_stream(&self.source.vhost, &self.source.app, &self.source.stream)
    }
}

/// Receiver returned to an embedded media consumer.
pub struct EmbeddedSubscription {
    source: Arc<MediaSource>,
    cached: VecDeque<MediaFrame>,
    receiver: broadcast::Receiver<MediaFrame>,
}

impl EmbeddedSubscription {
    pub fn source_id(&self) -> &str {
        &self.source.id
    }

    pub async fn recv(&mut self) -> Result<MediaFrame, broadcast::error::RecvError> {
        if let Some(frame) = self.cached.pop_front() {
            return Ok(frame);
        }
        self.receiver.recv().await
    }
}

/// Serializable-by-construction data owned by the caller, without internal
/// locks or live handles.
#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub id: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub info: MediaInfo,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl SourceSnapshot {
    async fn from_source(source: &MediaSource) -> Self {
        Self {
            id: source.id.clone(),
            vhost: source.vhost.clone(),
            app: source.app.clone(),
            stream: source.stream.clone(),
            info: source.info.read().await.clone(),
            bytes_in: source.bytes_in.load(Ordering::Relaxed),
            bytes_out: source.bytes_out.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CodecId, VideoInfo};
    use bytes::Bytes;

    fn video_frame(timestamp: u32, key_frame: bool) -> MediaFrame {
        MediaFrame::new_video(
            0,
            CodecId::H264,
            timestamp,
            timestamp as u64,
            timestamp as u64,
            Bytes::from_static(if key_frame { b"key" } else { b"delta" }),
            key_frame,
        )
    }

    #[tokio::test]
    async fn embedded_publish_subscribe_snapshot_and_close() {
        let kit = EmbeddedMediaKit::default();
        let publisher = kit.publisher("__defaultVhost__", "live", "camera");
        publisher
            .set_tracks(vec![TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 1920,
                height: 1080,
                fps: 25.0,
                key_frame: false,
            })])
            .await;
        publisher.publish(video_frame(0, true)).await;

        let mut subscriber = kit
            .subscribe("__defaultVhost__", "live", "camera")
            .await
            .expect("published stream should be subscribable");
        assert_eq!(subscriber.source_id(), "__defaultVhost__/live/camera");
        assert_eq!(subscriber.recv().await.unwrap().timestamp, 0);

        publisher.publish(video_frame(40, false)).await;
        assert_eq!(subscriber.recv().await.unwrap().timestamp, 40);

        let snapshots = kit.list_sources().await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].info.tracks.len(), 1);
        assert!(snapshots[0].bytes_in > 0);

        assert!(publisher.unpublish());
        assert!(subscriber.recv().await.is_err());
        assert!(kit.list_sources().await.is_empty());
    }
}
