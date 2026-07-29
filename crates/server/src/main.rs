use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use tokio::sync::mpsc;
use tokio::sync::Notify;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::config::{RecordConfig, ServerConfig};
use zlmediakit_core::event_bus::{Event, EventBus};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};
use zlmediakit_core::recorder::{RecorderCommand, RecorderControl};
use zlmediakit_core::stream_proxy::{ProxyCmd, ProxyTable, StreamProxyControl};
use zlmediakit_flv::FlvRecorder;
use zlmediakit_hls::HlsRecorder;
use zlmediakit_http::HttpServer;
use zlmediakit_mp4::recorder::Mp4Recorder;
use zlmediakit_rtmp::RtmpServer;
use zlmediakit_rtsp::rtsp_pull_start;
use zlmediakit_rtsp::RtspServer;
use zlmediakit_webrtc::WebRtcServer;

#[derive(Parser, Debug)]
#[command(name = "zlmediakit")]
#[command(about = "ZLMediaKit-RS - High performance streaming media server in Rust")]
struct Cli {
    #[arg(long, default_value = "conf/config.toml")]
    config: String,

    #[arg(long)]
    rtmp_port: Option<u16>,

    #[arg(long)]
    rtsp_port: Option<u16>,

    #[arg(long)]
    http_port: Option<u16>,

    #[arg(short = 'a', long)]
    api_port: Option<u16>,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long)]
    webrtc_port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
        )
        .init();

    let mut config = if std::path::Path::new(&cli.config).exists() {
        match ServerConfig::load(&cli.config) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to load config {}: {}", cli.config, e);
                ServerConfig::default()
            }
        }
    } else {
        info!("Config file {} not found, using defaults", cli.config);
        ServerConfig::default()
    };

    // The config file expresses per-protocol ports under `[rtmp] port` etc.,
    // while the top-level `rtmp_port` fields are the canonical values used
    // below. Promote the nested (optional) ports when present so that both
    // styles of configuration take effect.
    if let Some(p) = config.rtmp.port {
        config.rtmp_port = p;
    }
    if let Some(p) = config.rtsp.port {
        config.rtsp_port = p;
    }
    if let Some(p) = config.http.port {
        config.http_port = p;
    }

    if let Some(port) = cli.rtmp_port {
        config.rtmp_port = port;
    }
    if let Some(port) = cli.rtsp_port {
        config.rtsp_port = port;
    }
    if let Some(port) = cli.http_port {
        config.http_port = port;
    }
    if let Some(port) = cli.api_port {
        config.api_port = port;
    }
    if let Some(port) = cli.webrtc_port {
        config.webrtc_port = port;
    }

    let source_manager = Arc::new(MediaSourceManager::new());
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(config.auth_enabled, config.secret.clone());
    let (recorder_control, recorder_cmd_rx) = RecorderControl::new();
    let recorder_control = Arc::new(recorder_control);

    let (proxy_control, proxy_active, proxy_cmd_rx) = StreamProxyControl::new();
    let proxy_control = Arc::new(proxy_control);

    info!("========================================");
    info!("  ZLMediaKit-RS v{}", env!("CARGO_PKG_VERSION"));
    info!("  High Performance Streaming Server");
    info!("========================================");
    info!("RTMP port: {}", config.rtmp_port);
    info!("RTSP port: {}", config.rtsp_port);
    info!("HTTP port: {}", config.http_port);
    info!("API port:  {}", config.api_port);

    let mut handles = Vec::new();

    // Common TLS config (shared across protocols). When a protocol has
    // `ssl: true` its server will attempt TLS negotiation after accepting a
    // plain TCP connection.
    let ssl_cert = config.http.ssl_cert.clone();
    let ssl_key = config.http.ssl_key.clone();

    if config.rtmp.enabled {
        let addr = format!("0.0.0.0:{}", config.rtmp_port);
        let sm = source_manager.clone();
        let eb = event_bus.clone();
        let rtmp_auth = auth.clone();
        let rtmp_cert = config.rtmp.ssl_cert.clone();
        let rtmp_key = config.rtmp.ssl_key.clone();
        let handle = tokio::spawn(async move {
            match RtmpServer::new(&addr, sm, eb, rtmp_auth, rtmp_cert, rtmp_key).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("RTMP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start RTMP server: {}", e),
            }
        });
        handles.push(handle);
    }

    if config.rtsp.enabled {
        let addr = format!("0.0.0.0:{}", config.rtsp_port);
        let sm = source_manager.clone();
        let eb = event_bus.clone();
        let rtsp_auth = auth.clone();
        let rtsp_cert = config.rtsp.ssl_cert.clone();
        let rtsp_key = config.rtsp.ssl_key.clone();
        let handle = tokio::spawn(async move {
            match RtspServer::new(&addr, sm, eb, rtsp_auth, rtsp_cert, rtsp_key).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("RTSP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start RTSP server: {}", e),
            }
        });
        handles.push(handle);
    }

    if config.http.enabled {
        let addr = format!("0.0.0.0:{}", config.http_port);
        let sm = source_manager.clone();
        let http_auth = auth.clone();
        let rec = recorder_control.clone();
        let proxy = proxy_control.clone();
        let http_cert = ssl_cert.clone();
        let http_key = ssl_key.clone();
        let handle = tokio::spawn(async move {
            match HttpServer::new(&addr, sm, http_auth, rec, proxy, http_cert, http_key).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("HTTP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start HTTP server: {}", e),
            }
        });
        handles.push(handle);
    }

    // The API is served by the same HttpServer implementation (it handles
    // both /index/api/* and media playback). Bind it on its own port too so
    // the --api-port flag is honoured, unless it collides with http_port.
    if config.http.enabled && config.api_port != config.http_port {
        let addr = format!("0.0.0.0:{}", config.api_port);
        let sm = source_manager.clone();
        let api_auth = auth.clone();
        let rec = recorder_control.clone();
        let proxy = proxy_control.clone();
        let api_cert = ssl_cert.clone();
        let api_key = ssl_key.clone();
        let handle = tokio::spawn(async move {
            match HttpServer::new(&addr, sm, api_auth, rec, proxy, api_cert, api_key).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("API server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start API server: {}", e),
            }
        });
        handles.push(handle);
    }

    // WebRTC (WHEP) playback server: lets a browser play a published stream
    // over WebRTC. It binds its own HTTP listener (WHEP is HTTP-based) so the
    // `http` protocol crate stays decoupled from the `webrtc` crate.
    if config.webrtc.enabled {
        let addr = format!("0.0.0.0:{}", config.webrtc_port);
        let sm = source_manager.clone();
        let ice_servers = config.webrtc.ice_servers.clone();
        let handle = tokio::spawn(async move {
            match WebRtcServer::new(&addr, sm, ice_servers).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("WebRTC server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start WebRTC server: {}", e),
            }
        });
        handles.push(handle);
    }

    // Recorder supervisor: always runs so the HTTP API can start/stop recording
    // on demand. It also auto-records every published stream when `record.hls`
    // or `record.flv` is enabled in the config.
    {
        let rec_cfg = config.record.clone();
        let rec_base = if config.record.path.is_empty() {
            "./record".to_string()
        } else {
            config.record.path.clone()
        };
        let sup_event = event_bus.clone();
        let sup_sm = source_manager.clone();
        info!(
            "Recorder supervisor started (auto hls={}, flv={}, mp4={}) -> {}",
             config.record.hls, config.record.flv, config.record.mp4, rec_base
        );
        let rec = recorder_control.clone();
        let handle = tokio::spawn(async move {
            run_recorder_supervisor(sup_event, sup_sm, rec_cfg, rec_base, recorder_cmd_rx, rec)
                .await;
        });
        handles.push(handle);
    }

    // Stream proxy supervisor: drains `addStreamProxy` commands and spawns
    // pull tasks. Each task republishes a remote stream locally; when it ends,
    // the supervisor removes the entry from the shared active table.
    {
        let sup_sm = source_manager.clone();
        let sup_active = proxy_active.clone();
        info!("Stream proxy supervisor started");
        let handle = tokio::spawn(async move {
            run_proxy_supervisor(proxy_cmd_rx, sup_sm, sup_active).await;
        });
        handles.push(handle);
    }

    info!("All servers started. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    for handle in handles {
        handle.abort();
    }

    Ok(())
}

/// Watches the EventBus for stream publish/unpublish events and manages the
/// lifecycle of per-stream recorders. Each active stream maps to a `Notify`
/// that is signalled on unpublish (or on an explicit stop command) to
/// gracefully stop its recorder(s). On-demand start/stop requests arrive over
/// `cmd_rx` from the HTTP API.
async fn run_recorder_supervisor(
    event_bus: Arc<EventBus>,
    source_manager: Arc<MediaSourceManager>,
    record: RecordConfig,
    base_path: String,
    mut cmd_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    control: Arc<RecorderControl>,
) {
    let mut rx = event_bus.subscribe();
    let stops: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<Notify>>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(Event::StreamPublish { source_id, vhost, app, stream }) => {
                        let (hls, flv, mp4) = (record.hls, record.flv, record.mp4);
                        if (hls || flv || mp4) && !stops.lock().await.contains_key(&source_id) {
                            if let Some(src) = source_manager.get(&vhost, &app, &stream) {
                                let stop = Arc::new(Notify::new());
                                stops.lock().await.insert(source_id.clone(), stop.clone());
                                control.mark_recording(&source_id, hls, flv, mp4);
                                spawn_recorders(src, &base_path, hls, flv, mp4, stop).await;
                            }
                        }
                    }
                    Ok(Event::StreamUnPublish { source_id, .. }) => {
                        if let Some(stop) = stops.lock().await.remove(&source_id) {
                            stop.notify_waiters();
                        }
                        control.unmark_recording(&source_id);
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(RecorderCommand { vhost, app, stream, hls, flv, mp4 }) => {
                        let source_id = format!("{}/{}/{}", vhost, app, stream);
                        if hls || flv || mp4 {
                            if !stops.lock().await.contains_key(&source_id) {
                                if let Some(src) =
                                    source_manager.get(&vhost, &app, &stream)
                                {
                                    let stop = Arc::new(Notify::new());
                                    stops.lock().await.insert(source_id.clone(), stop.clone());
                                    control.mark_recording(&source_id, hls, flv, mp4);
                                    spawn_recorders(src, &base_path, hls, flv, mp4, stop).await;
                                } else {
                                    warn!("startRecord: stream not live: {}", source_id);
                                }
                            }
                        } else {
                            // Stop request.
                            if let Some(stop) = stops.lock().await.remove(&source_id) {
                                stop.notify_waiters();
                            }
                            control.unmark_recording(&source_id);
                        }
                    }
                    None => break, // command channel closed; shut down supervisor.
                }
            }
        }
    }
}

/// Spawns the FLV, HLS and/or MP4 recorder tasks for a live source, each
/// stopped via the shared `Notify` when the stream unpublishes or recording is
/// cancelled.
async fn spawn_recorders(
    source: Arc<MediaSource>,
    base_path: &str,
    hls: bool,
    flv: bool,
    mp4: bool,
    stop: Arc<Notify>,
) {
    if flv {
        let src = source.clone();
        let path = base_path.to_string();
        let s = stop.clone();
        tokio::spawn(async move {
            if let Err(e) = FlvRecorder::record(src, &path, s).await {
                error!("FLV recorder error: {}", e);
            }
        });
    }
    if hls {
        let src = source.clone();
        let path = base_path.to_string();
        let s = stop.clone();
        tokio::spawn(async move {
            if let Err(e) = HlsRecorder::record(src, &path, s).await {
                error!("HLS recorder error: {}", e);
            }
        });
    }
    if mp4 {
        let src = source.clone();
        let path = base_path.to_string();
        let s = stop.clone();
        tokio::spawn(async move {
            if let Err(e) = Mp4Recorder::record(src, &path, s).await {
                error!("MP4 recorder error: {}", e);
            }
        });
    }
}

/// Drains `ProxyCmd` commands from the HTTP API and spawns an RTSP pull task
/// for each. When a task finishes (stopped via `delStreamProxy`, or the
/// upstream drops) its entry is removed from the shared active table so
/// `getStreamProxyList` reflects reality.
async fn run_proxy_supervisor(
    mut cmd_rx: mpsc::UnboundedReceiver<ProxyCmd>,
    source_manager: Arc<MediaSourceManager>,
    active: ProxyTable,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        let ProxyCmd {
            url,
            vhost,
            app,
            stream,
            stop,
            stopped,
        } = cmd;
        let key = format!("{}/{}/{}", vhost, app, stream);
        let sm = source_manager.clone();
        let active = active.clone();
        tokio::spawn(async move {
            info!("stream proxy task start: {} -> {}", url, key);
            if let Err(e) = rtsp_pull_start(&url, &vhost, &app, &stream, sm, stop, stopped).await {
                warn!("stream proxy task error {}: {}", key, e);
            }
            info!("stream proxy task end: {}", key);
            active.remove(&key);
        });
    }
}
