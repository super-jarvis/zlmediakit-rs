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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_receive() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(Event::StreamPublish {
            source_id: "test".into(),
            vhost: "vhost".into(),
            app: "app".into(),
            stream: "stream".into(),
        });
        let event = rx.recv().await.unwrap();
        match event {
            Event::StreamPublish {
                source_id,
                vhost,
                app,
                stream,
            } => {
                assert_eq!(source_id, "test");
                assert_eq!(vhost, "vhost");
                assert_eq!(app, "app");
                assert_eq!(stream, "stream");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(Event::NoReader {
            source_id: "s1".into(),
        });
        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert!(matches!(e1, Event::NoReader { .. }));
        assert!(matches!(e2, Event::NoReader { .. }));
    }

    #[tokio::test]
    async fn register_listener_creates_entry() {
        let bus = EventBus::new(16);
        let rx = bus.register_listener("test-listener", 16);
        // register_listener creates an isolated channel; publish() only
        // sends to the internal tx, not to registered listeners.
        assert!(bus.listeners.contains_key("test-listener"));
        drop(rx);
    }

    #[tokio::test]
    async fn stream_not_found_event() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(Event::StreamNotFound {
            vhost: "v".into(),
            app: "a".into(),
            stream: "s".into(),
        });
        let event = rx.recv().await.unwrap();
        match event {
            Event::StreamNotFound { vhost, app, stream } => {
                assert_eq!(&vhost, "v");
                assert_eq!(&app, "a");
                assert_eq!(&stream, "s");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[test]
    fn default_capacity() {
        let bus = EventBus::default();
        let _rx = bus.subscribe();
        // Should not panic or hang
        bus.publish(Event::StreamUnPublish {
            source_id: "x".into(),
            vhost: "v".into(),
            app: "a".into(),
            stream: "s".into(),
        });
    }

    #[test]
    fn event_bus_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<EventBus>();
        assert_sync::<EventBus>();
        assert_send::<Event>();
        assert_sync::<Event>();
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}
