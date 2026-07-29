use super::session::RtspSession;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::transport::{load_tls_config, TlsAcceptor, TransportStream};

pub struct RtspServer {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    source_manager: Arc<MediaSourceManager>,
    event_bus: Arc<EventBus>,
    auth: Arc<StreamAuth>,
}

impl RtspServer {
    pub async fn new(
        addr: &str,
        source_manager: Arc<MediaSourceManager>,
        event_bus: Arc<EventBus>,
        auth: Arc<StreamAuth>,
        ssl_cert: Option<String>,
        ssl_key: Option<String>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let tls = load_tls_config(ssl_cert.as_deref(), ssl_key.as_deref())?;
        if tls.is_some() {
            info!("RTSPS server listening on {}", addr);
        } else {
            info!("RTSP server listening on {}", addr);
        }
        Ok(Self {
            listener,
            tls,
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

                    if let Some(ref tls) = self.tls {
                        let peer2 = peer.clone();
                        tokio::spawn(async move {
                            match tls.accept(stream).await {
                                Ok(tls_stream) => {
                                    info!("RTSPS connection from {}", peer2);
                                    let mut session = RtspSession::new(
                                        TransportStream::Tls(tls_stream),
                                        peer2,
                                        source_manager,
                                        event_bus,
                                        auth,
                                    );
                                    if let Err(e) = session.run().await {
                                        error!("RTSPS session error: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("TLS accept from {} failed: {}", peer2, e);
                                }
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            info!("RTSP connection from {}", peer);
                            let mut session = RtspSession::new(
                                TransportStream::Tcp(stream),
                                peer,
                                source_manager,
                                event_bus,
                                auth,
                            );
                            if let Err(e) = session.run().await {
                                error!("RTSP session error: {}", e);
                            }
                        });
                    }
                }
                Err(e) => {
                    error!("RTSP accept error: {}", e);
                }
            }
        }
    }
}
