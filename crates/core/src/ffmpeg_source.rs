use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Command sent from `FFmpegSourceControl` to the server-side supervisor
/// that spawns ffmpeg subprocesses.
#[derive(Debug)]
pub struct FFmpegCmd {
    pub src_url: String,
    pub dst_url: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub timeout_ms: u32,
}

/// Metadata for an active ffmpeg source.
#[derive(Clone)]
pub struct FFmpegEntry {
    pub src_url: String,
    pub dst_url: String,
}

/// Shared table of active ffmpeg sources.
pub type FFmpegTable = Arc<DashMap<String, FFmpegEntry>>;

/// Control handle shared with the HTTP API. Lets callers start/stop/list
/// ffmpeg-based stream sources.
#[derive(Clone)]
pub struct FFmpegSourceControl {
    active: Arc<DashMap<String, FFmpegEntry>>,
    tx: mpsc::UnboundedSender<FFmpegCmd>,
}

impl Default for FFmpegSourceControl {
    fn default() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            active: Arc::new(DashMap::new()),
            tx,
        }
    }
}

impl FFmpegSourceControl {
    pub fn new() -> (Self, FFmpegTable, mpsc::UnboundedReceiver<FFmpegCmd>) {
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

    pub fn add(
        &self,
        src_url: &str,
        dst_url: &str,
        vhost: &str,
        app: &str,
        stream: &str,
        timeout_ms: u32,
    ) -> bool {
        let key = Self::key(vhost, app, stream);
        if self.active.contains_key(&key) {
            return false;
        }
        self.active.insert(
            key,
            FFmpegEntry {
                src_url: src_url.to_string(),
                dst_url: dst_url.to_string(),
            },
        );
        let _ = self.tx.send(FFmpegCmd {
            src_url: src_url.to_string(),
            dst_url: dst_url.to_string(),
            vhost: vhost.to_string(),
            app: app.to_string(),
            stream: stream.to_string(),
            timeout_ms,
        });
        true
    }

    pub fn remove(&self, vhost: &str, app: &str, stream: &str) -> bool {
        self.active.remove(&Self::key(vhost, app, stream)).is_some()
    }

    pub fn list(&self) -> Vec<(String, String, String, String, String)> {
        self.active
            .iter()
            .map(|e| {
                let (v, a, s) = split_key(e.key());
                (
                    e.value().src_url.clone(),
                    e.value().dst_url.clone(),
                    v,
                    a,
                    s,
                )
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
