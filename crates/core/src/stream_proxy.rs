use dashmap::{mapref::entry::Entry, DashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

/// Command sent from `StreamProxyControl` to the server-side supervisor that
/// actually spawns the pull tasks. The supervisor lives in the binary crate so
/// it can depend on the protocol crates; this module stays protocol-agnostic.
#[derive(Debug)]
pub struct ProxyCmd {
    pub url: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub stop: Arc<Notify>,
    pub stopped: Arc<AtomicBool>,
}

/// A currently-active stream proxy entry (the stop signal lives here so the
/// control handle can cancel it on demand).
#[derive(Clone)]
pub struct ProxyEntry {
    pub url: String,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    on_demand: bool,
}

impl ProxyEntry {
    /// Returns whether this entry belongs to the indicated supervisor task.
    /// Used to prevent an old task from removing a newer proxy with the same
    /// local stream key after a rapid stop/start cycle.
    pub fn belongs_to(&self, stopped: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(&self.stopped, stopped)
    }

    pub fn is_on_demand(&self) -> bool {
        self.on_demand
    }
}

/// Shared table of active proxies, handed to the server supervisor so it can
/// clean up entries when a pull task ends.
pub type ProxyTable = Arc<DashMap<String, ProxyEntry>>;

/// Control handle shared with the HTTP API. Lets callers add/remove/list
/// stream proxies without depending on any protocol crate.
#[derive(Clone)]
pub struct StreamProxyControl {
    active: Arc<DashMap<String, ProxyEntry>>,
    tx: mpsc::UnboundedSender<ProxyCmd>,
}

impl StreamProxyControl {
    /// Creates the control handle plus the shared active-table and the command
    /// receiver the server supervisor should drain.
    pub fn new() -> (Self, ProxyTable, mpsc::UnboundedReceiver<ProxyCmd>) {
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

    /// Starts proxying `url` into the local stream `(vhost, app, stream)`.
    /// Returns `false` if a proxy for that stream already exists.
    pub fn add(&self, url: &str, vhost: &str, app: &str, stream: &str) -> bool {
        self.add_inner(url, vhost, app, stream, false)
    }

    /// Starts a transient edge pull that may be stopped when its last reader
    /// leaves. Manually configured/API-created proxies remain persistent.
    pub fn add_on_demand(&self, url: &str, vhost: &str, app: &str, stream: &str) -> bool {
        self.add_inner(url, vhost, app, stream, true)
    }

    fn add_inner(&self, url: &str, vhost: &str, app: &str, stream: &str, on_demand: bool) -> bool {
        let key = Self::key(vhost, app, stream);
        let stop = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        match self.active.entry(key.clone()) {
            Entry::Occupied(_) => return false,
            Entry::Vacant(entry) => entry.insert(ProxyEntry {
                url: url.to_string(),
                stop: stop.clone(),
                stopped: stopped.clone(),
                on_demand,
            }),
        };
        let task_marker = stopped.clone();
        if self
            .tx
            .send(ProxyCmd {
                url: url.to_string(),
                vhost: vhost.to_string(),
                app: app.to_string(),
                stream: stream.to_string(),
                stop,
                stopped,
            })
            .is_err()
        {
            self.active
                .remove_if(&key, |_, entry| entry.belongs_to(&task_marker));
            return false;
        }
        true
    }

    /// Stops the proxy for `(vhost, app, stream)`, if any. Returns `true` if a
    /// proxy was active and has been signalled to stop.
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

    pub fn is_active(&self, vhost: &str, app: &str, stream: &str) -> bool {
        self.active.contains_key(&Self::key(vhost, app, stream))
    }

    pub fn is_on_demand(&self, vhost: &str, app: &str, stream: &str) -> bool {
        self.active
            .get(&Self::key(vhost, app, stream))
            .is_some_and(|entry| entry.is_on_demand())
    }

    /// Returns all active proxies as `(url, vhost, app, stream)` tuples.
    pub fn list(&self) -> Vec<(String, String, String, String)> {
        self.active
            .iter()
            .map(|e| {
                let (v, a, s) = split_key(e.key());
                (e.value().url.clone(), v, a, s)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn concurrent_add_starts_exactly_one_proxy() {
        let (control, _, mut rx) = StreamProxyControl::new();
        let control = Arc::new(control);
        let barrier = Arc::new(Barrier::new(17));
        let mut workers = Vec::new();
        for _ in 0..16 {
            let control = control.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                control.add("rtmp://origin/live/camera", "v", "live", "camera")
            }));
        }
        barrier.wait();

        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|success| *success)
            .count();
        assert_eq!(successes, 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
        assert_eq!(control.list().len(), 1);
    }

    #[test]
    fn add_rolls_back_when_supervisor_is_gone() {
        let (control, _, rx) = StreamProxyControl::new();
        drop(rx);

        assert!(!control.add("rtmp://origin/live/camera", "v", "live", "camera"));
        assert!(control.list().is_empty());
    }
}
