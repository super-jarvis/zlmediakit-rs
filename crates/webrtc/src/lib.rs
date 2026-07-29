//! WebRTC support for zlmediakit-rs.
//!
//! Transport (ICE / DTLS / SRTP / SDP) is provided by the `webrtc` crate. This
//! crate focuses on the protocol glue: exposing a stream published into the
//! shared `MediaSourceManager` as a WebRTC playback session (WHEP), packetizing
//! its frames into RTP and sending them over SRTP.
//!
//! Scope of the *minimal skeleton*: WHEP playback only (a browser plays a
//! server-side stream). Publishing via WebRTC (WHIP) is left for a later
//! iteration.

pub mod server;
pub mod whep;

pub use server::WebRtcServer;
pub use whep::{whep_play, WhepSession};
