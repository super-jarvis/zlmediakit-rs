use super::session::HttpSession;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_core::transport::{load_tls_config, TlsAcceptor, TransportStream};

pub struct HttpServer {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    source_manager: Arc<MediaSourceManager>,
    auth: Arc<StreamAuth>,
    recorder: Arc<RecorderControl>,
    proxy: Arc<StreamProxyControl>,
}

impl HttpServer {
    pub async fn new(
        addr: &str,
        source_manager: Arc<MediaSourceManager>,
        auth: Arc<StreamAuth>,
        recorder: Arc<RecorderControl>,
        proxy: Arc<StreamProxyControl>,
        ssl_cert: Option<String>,
        ssl_key: Option<String>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let tls = load_tls_config(ssl_cert.as_deref(), ssl_key.as_deref())?;
        if tls.is_some() {
            info!("HTTPS server listening on {}", addr);
        } else {
            info!("HTTP server listening on {}", addr);
        }
        Ok(Self {
            listener,
            tls,
            source_manager,
            auth,
            recorder,
            proxy,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let source_manager = self.source_manager.clone();
                    let auth = self.auth.clone();
                    let recorder = self.recorder.clone();
                    let proxy = self.proxy.clone();
                    let peer = peer_addr.to_string();

                    if let Some(ref tls) = self.tls {
                        let peer2 = peer.clone();
                        tokio::spawn(async move {
                            match tls.accept(stream).await {
                                Ok(tls_stream) => {
                                    let mut session = HttpSession::new(
                                        TransportStream::Tls(tls_stream),
                                        peer2,
                                        source_manager,
                                        auth,
                                        recorder,
                                        proxy,
                                    );
                                    if let Err(e) = session.run().await {
                                        error!("HTTPS session error: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("TLS accept from {} failed: {}", peer2, e);
                                }
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            let mut session = HttpSession::new(
                                TransportStream::Tcp(stream),
                                peer,
                                source_manager,
                                auth,
                                recorder,
                                proxy,
                            );
                            if let Err(e) = session.run().await {
                                error!("HTTP session error: {}", e);
                            }
                        });
                    }
                }
                Err(e) => {
                    error!("HTTP accept error: {}", e);
                }
            }
        }
    }
}
