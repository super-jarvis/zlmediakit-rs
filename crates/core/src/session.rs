use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

pub type SessionId = String;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: SessionId,
    pub peer_addr: String,
    /// Local socket address (ip:port) this session is attached to.
    pub local_addr: String,
    pub protocol: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Optional stream binding (vhost, app, stream) recorded when this
    /// session publishes or plays a specific stream. Enables kicking all
    /// sessions of a stream at once.
    pub stream: Option<(String, String, String)>,
}

pub struct SessionManager {
    sessions: DashMap<SessionId, SessionInfo>,
    counter: AtomicU64,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            counter: AtomicU64::new(0),
        }
    }

    pub fn create(&self, peer_addr: String, protocol: String) -> SessionId {
        self.create_with_local(peer_addr, String::new(), protocol)
    }

    pub fn create_with_local(
        &self,
        peer_addr: String,
        local_addr: String,
        protocol: String,
    ) -> SessionId {
        let id = format!(
            "{}-{}",
            protocol.to_uppercase(),
            self.counter.fetch_add(1, Ordering::SeqCst)
        );
        let info = SessionInfo {
            id: id.clone(),
            peer_addr: peer_addr.clone(),
            local_addr,
            protocol,
            created_at: chrono::Utc::now(),
            stream: None,
        };
        self.sessions.insert(id.clone(), info);
        info!("Session created: {} from {}", id, peer_addr);
        id
    }

    /// Records the stream a session is attached to (publish or play).
    pub fn bind_stream(&self, id: &SessionId, vhost: &str, app: &str, stream: &str) {
        if let Some(mut info) = self.sessions.get_mut(id) {
            info.stream = Some((vhost.to_string(), app.to_string(), stream.to_string()));
        }
    }

    pub fn remove(&self, id: &SessionId) {
        if let Some(info) = self.sessions.remove(id).map(|(_, v)| v) {
            info!("Session removed: {} from {}", id, info.peer_addr);
        }
    }

    /// Kicks a single session by its id. Returns `true` if it existed.
    pub fn kick(&self, id: &str) -> bool {
        let existed = self.sessions.contains_key(id);
        self.sessions.remove(id);
        if existed {
            info!("Session kicked: {}", id);
        }
        existed
    }

    /// Kicks every session bound to the given stream. Returns the number
    /// of sessions kicked.
    pub fn kick_by_stream(&self, vhost: &str, app: &str, stream: &str) -> usize {
        let ids: Vec<SessionId> = self
            .sessions
            .iter()
            .filter_map(|entry| {
                let info = entry.value();
                match &info.stream {
                    Some((v, a, s)) if v == vhost && a == app && s == stream => {
                        Some(info.id.clone())
                    }
                    _ => None,
                }
            })
            .collect();
        let n = ids.len();
        for id in &ids {
            self.remove(id);
        }
        n
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionInfo> {
        self.sessions.get(id).map(|s| s.clone())
    }

    /// Kicks every session matching the given filters (all optional).
    /// `peer_ip` and `local_port` filter on the respective socket parts;
    /// `protocol` filters on the session type (e.g. `HTTP`).
    /// Returns the number of sessions kicked.
    pub fn kick_by_filter(
        &self,
        peer_ip: Option<&str>,
        local_port: Option<u16>,
        protocol: Option<&str>,
    ) -> usize {
        let ids: Vec<SessionId> = self
            .sessions
            .iter()
            .filter_map(|entry| {
                let info = entry.value();
                let peer_ip_ok = peer_ip.is_none_or(|ip| {
                    info.peer_addr.split(':').next().unwrap_or("") == ip
                });
                let local_port_ok = local_port.is_none_or(|p| {
                    info.local_addr
                        .split(':')
                        .next_back()
                        .and_then(|s| s.parse::<u16>().ok())
                        == Some(p)
                });
                let protocol_ok = protocol.is_none_or(|p| info.protocol == p);
                if peer_ip_ok && local_port_ok && protocol_ok {
                    Some(info.id.clone())
                } else {
                    None
                }
            })
            .collect();
        let n = ids.len();
        for id in &ids {
            self.remove(id);
        }
        n
    }

    /// Returns a snapshot of all live sessions.
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|e| e.value().clone()).collect()
    }

    pub fn count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
