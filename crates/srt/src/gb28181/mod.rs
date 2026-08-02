//! GB28181 SIP signaling + RTP/PS receive support.
//!
//! A GB28181 device `REGISTER`s with the SIP server; the platform can then
//! `INVITE` the device (or the device can push via `INVITE`) to send its
//! MPEG-PS stream over RTP to a local media port. The [`RtpServerManager`]
//! demuxes the PS stream and republishes it as a `MediaSource`, so the stream
//! becomes playable over RTMP/RTSP/HTTP-FLV/HLS/WebRTC like any other.

pub mod rtp_server;
pub mod sip;
pub mod xml;

pub use rtp_server::{
    RtpPayloadType, RtpServerInfo, RtpServerManager, RtpStreamReceiver, RtpTcpMode,
};
pub use sip::{ChannelInfo, DeviceInfo, SipInfo, SipServer, SipServerConfig, TalkInfo};

use std::sync::Arc;

use zlmediakit_core::{Gb28181Config, MediaSourceManager};

/// High-level GB28181 server: starts the SIP signaling loop and exposes the
/// [`RtpServerManager`] used to receive PS streams.
pub struct Gb28181Server {
    pub sip: Arc<SipServer>,
    pub rtp: Arc<RtpServerManager>,
}

impl Gb28181Server {
    /// Start the SIP server and wire it to the shared media manager.
    pub async fn start(
        config: &Gb28181Config,
        manager: Arc<MediaSourceManager>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_with_bind_ip(
            config,
            manager,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        )
        .await
    }

    pub async fn start_with_bind_ip(
        config: &Gb28181Config,
        manager: Arc<MediaSourceManager>,
        bind_ip: std::net::IpAddr,
    ) -> anyhow::Result<Arc<Self>> {
        let sip_cfg = SipServerConfig {
            sip_port: config.sip_port,
            realm: config.realm.clone(),
            password: config.password.clone(),
            host: config.host.clone(),
            media_port_base: config.media_port,
            vhost: "__defaultVhost__".to_string(),
        };
        let sip = SipServer::new_with_bind_ip(sip_cfg, manager, config.media_port, bind_ip).await?;
        let rtp = sip.rtp_manager();
        let sip_clone = sip.clone();
        tokio::spawn(async move {
            sip_clone.run().await;
        });
        Ok(Arc::new(Self { sip, rtp }))
    }
}
