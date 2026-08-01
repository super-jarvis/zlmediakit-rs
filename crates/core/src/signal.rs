//! Global process-lifecycle signals shared between the server binary and the
//! protocol handlers (e.g. the HTTP `restartServer` API).

use std::sync::{Arc, OnceLock};
use tokio::sync::Notify;

/// A process-wide shutdown signal. Signalled by the `restartServer` API (or any
/// other component that wants to request a graceful restart). The server's main
/// loop selects on this alongside `ctrl_c`.
pub static SHUTDOWN: OnceLock<Arc<Notify>> = OnceLock::new();

/// Returns the shared shutdown `Notify`, initializing it on first use.
pub fn shutdown_signal() -> &'static Arc<Notify> {
    SHUTDOWN.get_or_init(|| Arc::new(Notify::new()))
}

/// Requests a graceful process shutdown (which, under a supervisor, results in
/// a restart). Fail-open if nothing is waiting on the signal.
pub fn request_shutdown() {
    shutdown_signal().notify_one();
}
