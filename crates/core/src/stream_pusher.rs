use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

/// Command sent from `StreamPusherControl` to the server-side supervisor that
/// spawns push tasks. The supervisor lives in the binary crate so it can depend
/// on the protocol crates; this module stays protocol-agnostic.
#[derive(Debug)]
pub struct PusherCmd {
    pub dst_url: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub stop: Arc<Notify>,
    pub stopped: Arc<AtomicBool>,
}

/// A currently-active stream pusher entry.
#[derive(Clone)]
pub struct PusherEntry {
    pub dst_url: String,
    pub stop: Arc<Notify>,
    pub stopped: Arc<AtomicBool>,
}

/// Shared table of active pushers.
pub type PusherTable = Arc<DashMap<String, PusherEntry>>;

/// Control handle shared with the HTTP API. Lets callers push local streams
/// to remote RTMP(S) servers without depending on any protocol crate.
#[derive(Clone)]
pub struct StreamPusherControl {
    active: Arc<DashMap<String, PusherEntry>>,
    tx: mpsc::UnboundedSender<PusherCmd>,
}

impl Default for StreamPusherControl {
    fn default() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            active: Arc::new(DashMap::new()),
            tx,
        }
    }
}

impl StreamPusherControl {
    pub fn new() -> (Self, PusherTable, mpsc::UnboundedReceiver<PusherCmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let active = Arc::new(DashMap::new());
        (
            Self {
                active: active.clone(),
                tx,
            },
            active,
            rx,
        )
    }

    fn key(vhost: &str, app: &str, stream: &str) -> String {
        format!("{}/{}/{}", vhost, app, stream)
    }

    pub fn add(&self, dst_url: &str, vhost: &str, app: &str, stream: &str) -> bool {
        let key = Self::key(vhost, app, stream);
        if self.active.contains_key(&key) {
            return false;
        }
        let stop = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        self.active.insert(
            key,
            PusherEntry {
                dst_url: dst_url.to_string(),
                stop: stop.clone(),
                stopped: stopped.clone(),
            },
        );
        let _ = self.tx.send(PusherCmd {
            dst_url: dst_url.to_string(),
            vhost: vhost.to_string(),
            app: app.to_string(),
            stream: stream.to_string(),
            stop,
            stopped,
        });
        true
    }

    pub fn remove(&self, vhost: &str, app: &str, stream: &str) -> bool {
        let key = Self::key(vhost, app, stream);
        if let Some((_, entry)) = self.active.remove(&key) {
            entry.stopped.store(true, Ordering::SeqCst);
            entry.stop.notify_waiters();
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> Vec<(String, String, String, String)> {
        self.active
            .iter()
            .map(|e| {
                let (v, a, s) = split_key(e.key());
                (e.value().dst_url.clone(), v, a, s)
            })
            .collect()
    }
}

fn split_key(k: &str) -> (String, String, String) {
    let parts: Vec<&str> = k.splitn(3, '/').collect();
    if parts.len() == 3 {
        (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        )
    } else {
        (String::new(), String::new(), k.to_string())
    }
}
