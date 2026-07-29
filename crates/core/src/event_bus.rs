use crate::media_frame::MediaFrame;
use crate::media_source::SourceId;
use dashmap::DashMap;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum Event {
    StreamPublish {
        source_id: SourceId,
        vhost: String,
        app: String,
        stream: String,
    },
    StreamUnPublish {
        source_id: SourceId,
        vhost: String,
        app: String,
        stream: String,
    },
    StreamPlay {
        source_id: SourceId,
        session_id: String,
    },
    StreamStop {
        source_id: SourceId,
        session_id: String,
    },
    StreamNotFound {
        vhost: String,
        app: String,
        stream: String,
    },
    NoReader {
        source_id: SourceId,
    },
    FrameReceived {
        source_id: SourceId,
        frame: MediaFrame,
    },
}

pub struct EventBus {
    tx: broadcast::Sender<Event>,
    listeners: DashMap<String, broadcast::Sender<Event>>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            listeners: DashMap::new(),
        }
    }

    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn register_listener(&self, name: &str, capacity: usize) -> broadcast::Receiver<Event> {
        let (tx, rx) = broadcast::channel(capacity);
        self.listeners.insert(name.to_string(), tx);
        rx
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
