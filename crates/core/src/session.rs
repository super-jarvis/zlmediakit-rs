use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

pub type SessionId = String;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: SessionId,
    pub peer_addr: String,
    pub protocol: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
        let id = format!(
            "{}-{}",
            protocol.to_uppercase(),
            self.counter.fetch_add(1, Ordering::SeqCst)
        );
        let info = SessionInfo {
            id: id.clone(),
            peer_addr: peer_addr.clone(),
            protocol,
            created_at: chrono::Utc::now(),
        };
        self.sessions.insert(id.clone(), info);
        info!("Session created: {} from {}", id, peer_addr);
        id
    }

    pub fn remove(&self, id: &SessionId) {
        if let Some(info) = self.sessions.remove(id).map(|(_, v)| v) {
            info!("Session removed: {} from {}", id, info.peer_addr);
        }
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionInfo> {
        self.sessions.get(id).map(|s| s.clone())
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
