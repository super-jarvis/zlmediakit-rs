//! WebRTC support for zlmediakit-rs.
//!
//! Transport (ICE / DTLS / SRTP / SDP) is provided by the `webrtc` crate. This
//! crate focuses on the protocol glue: exposing a stream published into the
//! shared `MediaSourceManager` as a WebRTC playback session (WHEP), and
//! ingesting a WebRTC-published stream (WHIP) back into a `MediaSource`.
//!
//! Scope of the minimal skeleton:
//! * WHEP playback (server -> client): a browser plays a server-side stream.
//! * WHIP publishing (client -> server): a browser pushes a stream that any
//!   other protocol can then play.
//!
//! Both H.264 video and Opus audio are supported. AAC audio published by other
//! protocols cannot be carried by WebRTC directly; optional AAC->Opus
//! transcoding lives in the `transcode` module (behind the `transcode` Cargo
//! feature, which needs native `libopus`/`libfdk-aac` at build time).

pub mod datachannel;
pub mod engine;
pub mod h264_rtp;
pub mod server;
pub mod whep;
pub mod whip;

#[cfg(feature = "transcode")]
pub mod transcode;

pub use server::WebRtcServer;
pub use whep::{whep_play, WhepSession};
pub use whip::{whip_publish, WhipSession};

/// Install a default rustls `CryptoProvider`.
///
/// The `webrtc` crate depends on `rustls` with the `ring` provider enabled, but
/// other crates in this workspace (notably the HTTP/TLS stack) also enable
/// `aws-lc-rs` on the same `rustls` build. When both provider features are
/// present rustls will not auto-select one, and every DTLS/SRTP handshake fails
/// with *"Could not automatically determine the process-level CryptoProvider"*.
/// Calling this once (idempotent) before any `PeerConnection` is created forces
/// the `ring` provider as the process default.
pub fn init_crypto() {
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
}
