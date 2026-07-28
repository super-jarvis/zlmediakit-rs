use super::session::HttpSession;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use zlmediakit_core::media_source::MediaSourceManager;

pub struct HttpServer {
    listener: TcpListener,
    source_manager: Arc<MediaSourceManager>,
}

impl HttpServer {
    pub async fn new(addr: &str, source_manager: Arc<MediaSourceManager>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        info!("HTTP server listening on {}", addr);
        Ok(Self {
            listener,
            source_manager,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let source_manager = self.source_manager.clone();
                    let peer = peer_addr.to_string();

                    tokio::spawn(async move {
                        let mut session = HttpSession::new(stream, peer, source_manager);
                        if let Err(e) = session.run().await {
                            error!("HTTP session error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("HTTP accept error: {}", e);
                }
            }
        }
    }
}
