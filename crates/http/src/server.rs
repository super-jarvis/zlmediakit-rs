use super::session::HttpSession;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::ffmpeg_source::FFmpegSourceControl;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_core::stream_pusher::StreamPusherControl;
use zlmediakit_core::transport::{load_tls_config, TlsAcceptor, TransportStream};
use zlmediakit_srt::{RtpServerManager, SipServer};
use zlmediakit_transcode::TranscodeManager;

/// Bundled configuration for the HTTP server.
pub struct HttpServerConfig {
    pub addr: String,
    pub source_manager: Arc<MediaSourceManager>,
    pub auth: Arc<StreamAuth>,
    pub hook: Arc<HookClient>,
    pub recorder: Arc<RecorderControl>,
    pub proxy: Arc<StreamProxyControl>,
    pub pusher: Arc<StreamPusherControl>,
    pub ffmpeg: Arc<FFmpegSourceControl>,
    /// Optional GB28181 / openRtpServer RTP reception manager.
    pub rtp: Option<Arc<RtpServerManager>>,
    pub sip: Option<Arc<SipServer>>,
    /// Optional real-time transcoding manager (addTranscode API).
    pub transcode: Option<Arc<TranscodeManager>>,
    pub record_root: PathBuf,
    pub www_root: Option<PathBuf>,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
}

pub struct HttpServer {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    source_manager: Arc<MediaSourceManager>,
    auth: Arc<StreamAuth>,
    hook: Arc<HookClient>,
    recorder: Arc<RecorderControl>,
    proxy: Arc<StreamProxyControl>,
    pusher: Arc<StreamPusherControl>,
    ffmpeg: Arc<FFmpegSourceControl>,
    rtp: Option<Arc<RtpServerManager>>,
    sip: Option<Arc<SipServer>>,
    transcode: Option<Arc<TranscodeManager>>,
    record_root: PathBuf,
    www_root: Option<PathBuf>,
}

impl HttpServer {
    pub async fn new(config: HttpServerConfig) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(&config.addr).await?;
        let tls = load_tls_config(config.ssl_cert.as_deref(), config.ssl_key.as_deref())?;
        if tls.is_some() {
            info!("HTTPS server listening on {}", config.addr);
        } else {
            info!("HTTP server listening on {}", config.addr);
        }
        Ok(Self {
            listener,
            tls,
            source_manager: config.source_manager,
            auth: config.auth,
            hook: config.hook,
            recorder: config.recorder,
            proxy: config.proxy,
            pusher: config.pusher,
            ffmpeg: config.ffmpeg,
            rtp: config.rtp,
            sip: config.sip,
            transcode: config.transcode,
            record_root: config.record_root,
            www_root: config.www_root,
        })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let tls = self.tls.clone();
        loop {
            let record_root = self.record_root.clone();
            let www_root = self.www_root.clone();
            match self.listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let source_manager = self.source_manager.clone();
                    let auth = self.auth.clone();
                    let hook = self.hook.clone();
                    let recorder = self.recorder.clone();
                    let proxy = self.proxy.clone();
                    let pusher = self.pusher.clone();
                    let ffmpeg = self.ffmpeg.clone();
                    let rtp = self.rtp.clone();
                    let sip = self.sip.clone();
                    let transcode = self.transcode.clone();
                    let peer = peer_addr.to_string();

                    if let Some(ref tls) = tls {
                        let tls = tls.clone();
                        let peer2 = peer.clone();
                        tokio::spawn(async move {
                            match tls.accept(stream).await {
                                Ok(tls_stream) => {
                                    let session = HttpSession::new(
                                        TransportStream::from_tls_accepted(tls_stream),
                                        peer2,
                                        source_manager,
                                        auth,
                                        hook,
                                        recorder,
                                        proxy,
                                        pusher,
                                        ffmpeg,
                                        rtp.clone(),
                                        sip.clone(),
                                        transcode.clone(),
                                        record_root.clone(),
                                        www_root.clone(),
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
                            let session = HttpSession::new(
                                TransportStream::Tcp(stream),
                                peer,
                                source_manager,
                                auth,
                                hook,
                                recorder,
                                proxy,
                                pusher,
                                ffmpeg,
                                rtp.clone(),
                                sip.clone(),
                                transcode.clone(),
                                record_root.clone(),
                                www_root.clone(),
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
