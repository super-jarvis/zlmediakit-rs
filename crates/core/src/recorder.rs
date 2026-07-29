use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A request from the HTTP API to the recorder supervisor asking it to start or
/// stop recording of a specific (already live) stream on demand.
#[derive(Debug, Clone)]
pub struct RecorderCommand {
    pub vhost: String,
    pub app: String,
    pub stream: String,
    /// Record to HLS (`.ts` segments + `.m3u8`).
    pub hls: bool,
    /// Record to FLV.
    pub flv: bool,
    /// Record to MP4.
    pub mp4: bool,
}

/// Shared handle used by the HTTP API to drive on-demand recording and to
/// report recording state. The actual recorder tasks are owned by the
/// supervisor loop in the server binary, which drains the command channel and
/// keeps `state` in sync with the real lifecycle.
#[derive(Debug, Clone)]
pub struct RecorderControl {
    tx: mpsc::UnboundedSender<RecorderCommand>,
    state: Arc<DashMap<String, (bool, bool, bool)>>,
}

impl RecorderControl {
    /// Creates the control handle and the command receiver that must be handed
    /// to `run_recorder_supervisor`.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<RecorderCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                state: Arc::new(DashMap::new()),
            },
            rx,
        )
    }

    /// Asks the supervisor to start recording the given stream. Returns false
    /// only if the command channel is closed (server shutting down).
    pub fn start(&self, vhost: &str, app: &str, stream: &str, hls: bool, flv: bool, mp4: bool) -> bool {
        self.tx
            .send(RecorderCommand {
                vhost: vhost.to_string(),
                app: app.to_string(),
                stream: stream.to_string(),
                hls,
                flv,
                mp4,
            })
            .is_ok()
    }

    /// Asks the supervisor to stop recording the given stream.
    pub fn stop(&self, vhost: &str, app: &str, stream: &str) -> bool {
        self.tx
            .send(RecorderCommand {
                vhost: vhost.to_string(),
                app: app.to_string(),
                stream: stream.to_string(),
                hls: false,
                flv: false,
                mp4: false,
            })
            .is_ok()
    }

    /// Returns the current recording state of a stream as `(hls, flv, mp4)` flags,
    /// or `None` if it is not being recorded.
    pub fn is_recording(&self, vhost: &str, app: &str, stream: &str) -> Option<(bool, bool, bool)> {
        let id = format!("{}/{}/{}", vhost, app, stream);
        self.state.get(&id).map(|v| *v)
    }

    /// The supervisor calls these to keep `state` in sync with reality.
    pub fn mark_recording(&self, id: &str, hls: bool, flv: bool, mp4: bool) {
        self.state.insert(id.to_string(), (hls, flv, mp4));
    }

    pub fn unmark_recording(&self, id: &str) {
        self.state.remove(id);
    }
}
