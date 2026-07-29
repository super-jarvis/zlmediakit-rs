use super::session::RtmpSession;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;

pub struct RtmpServer {
    listener: TcpListener,
    source_manager: Arc<MediaSourceManager>,
    event_bus: Arc<EventBus>,
    auth: Arc<StreamAuth>,
}

impl RtmpServer {
    pub async fn new(
        addr: &str,
        source_manager: Arc<MediaSourceManager>,
        event_bus: Arc<EventBus>,
        auth: Arc<StreamAuth>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        info!("RTMP server listening on {}", addr);
        Ok(Self {
            listener,
            source_manager,
            event_bus,
            auth,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let source_manager = self.source_manager.clone();
                    let event_bus = self.event_bus.clone();
                    let auth = self.auth.clone();
                    let peer = peer_addr.to_string();
                    info!("RTMP connection from {}", peer);

                    tokio::spawn(async move {
                        let mut session =
                            RtmpSession::new(stream, peer, source_manager, event_bus, auth);
                        if let Err(e) = session.run().await {
                            error!("RTMP session error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("RTMP accept error: {}", e);
                }
            }
        }
    }
}
