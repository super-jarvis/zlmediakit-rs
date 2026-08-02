//! SRT (Secure Reliable Transport) ingest and GB28181 RTP/PS push
//! for zlmediakit-rs.

mod client;
pub mod ffi;
pub mod gb28181;
pub mod pull_client;
pub mod push_client;
pub mod server;

pub use gb28181::{
    ChannelInfo, Gb28181Server, RtpPayloadType, RtpServerInfo, RtpServerManager, RtpTcpMode,
    SipInfo, SipServer, SipServerConfig, TalkInfo,
};
pub use server::{SrtServer, SrtServerConfig, TsStreamDemuxer};
// Gb28181Config now lives in the core crate's config module.
pub use zlmediakit_core::Gb28181Config;
