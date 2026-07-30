//! SRT (Secure Reliable Transport) ingest and GB28181 RTP/PS push
//! for zlmediakit-rs.

pub mod ffi;
pub mod gb28181;
pub mod server;

pub use gb28181::{Gb28181Server, RtpPayloadType, RtpServerInfo, RtpServerManager, SipServer};
pub use server::{SrtServer, SrtServerConfig};
// Gb28181Config now lives in the core crate's config module.
pub use zlmediakit_core::Gb28181Config;
