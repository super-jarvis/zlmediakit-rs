use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio::sync::Notify;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::config::{RecordConfig, ServerConfig};
use zlmediakit_core::event_bus::{Event, EventBus};
use zlmediakit_core::ffmpeg_source::{FFmpegCmd, FFmpegSourceControl, FFmpegTable};
use zlmediakit_core::hook::{HookClient, HookResult};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};
use zlmediakit_core::recorder::{RecorderCommand, RecorderControl};
use zlmediakit_core::stream_proxy::{ProxyCmd, ProxyTable, StreamProxyControl};
use zlmediakit_core::stream_pusher::{PusherCmd, PusherTable, StreamPusherControl};
use zlmediakit_flv::FlvRecorder;
use zlmediakit_hls::HlsRecorder;
use zlmediakit_http::{HttpServer, HttpServerConfig};
use zlmediakit_mp4::recorder::Mp4Recorder;
use zlmediakit_rtmp::pull_client as rtmp_pull_client;
use zlmediakit_rtmp::push_client as rtmp_push_client;
use zlmediakit_rtmp::RtmpServer;
use zlmediakit_rtsp::rtsp_pull_start;
use zlmediakit_rtsp::RtspServer;
use zlmediakit_srt::{Gb28181Server, RtpServerManager, SipServer, SrtServer, SrtServerConfig};
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

    let event_bus = Arc::new(EventBus::new(1024));
    let source_manager = Arc::new(MediaSourceManager::new(Some(event_bus.clone())));
    let auth = StreamAuth::new(config.auth_enabled, config.secret.clone());
    let hook = HookClient::new(config.hook.clone());
    let (recorder_control, recorder_cmd_rx) = RecorderControl::new();
    let recorder_control = Arc::new(recorder_control);

    let (proxy_control, proxy_active, proxy_cmd_rx) = StreamProxyControl::new();
    let proxy_control = Arc::new(proxy_control);

    // Enable edge auto-pull when an origin URL is configured. A play request
    // for a stream that is not available locally will transparently pull it
    // from the origin server.
    if let Some(origin_url) = &config.cluster.origin_url {
        if !origin_url.is_empty() {
            source_manager.set_origin_pull(
                proxy_control.clone(),
                origin_url.clone(),
                config.cluster.origin_vhost.clone(),
                config.cluster.origin_app.clone(),
            );
            info!("edge auto-pull enabled, origin={}", origin_url);
        }
    }

    let (pusher_control, pusher_active, pusher_cmd_rx) = StreamPusherControl::new();
    let pusher_control = Arc::new(pusher_control);

    let (ffmpeg_control, ffmpeg_active, ffmpeg_cmd_rx) = FFmpegSourceControl::new();
    let ffmpeg_control = Arc::new(ffmpeg_control);

    info!("========================================");
    info!("  ZLMediaKit-RS v{}", env!("CARGO_PKG_VERSION"));
    info!("  High Performance Streaming Server");
    info!("========================================");
    info!("RTMP port: {}", config.rtmp_port);
    info!("RTSP port: {}", config.rtsp_port);
    info!("HTTP port: {}", config.http_port);
    info!("API port:  {}", config.api_port);

    let mut handles = Vec::new();

    // Recording root, shared by the HTTP VOD server and the recorder supervisor.
    let record_base = if config.record.path.is_empty() {
        "./record".to_string()
    } else {
        config.record.path.clone()
    };
    // Owned clones handed to the per-server tasks so each task owns its copy
    // (keeps the spawned futures `'static` and leaves `record_base` free to be
    // moved into the recorder supervisor).
    let http_record_root = record_base.clone();
    let api_record_root = record_base.clone();
    // Static file web root: only enabled when dir_root is true in config.
    // When `www_root` is empty/relative, fall back to the bundled `www/`
    // directory shipped with the source tree so `cargo run` works out of the
    // box. Absolute paths in config always win.
    let www_root = if config.http.dir_root {
        let cfg = config.http.www_root.trim();
        let p = if cfg.is_empty() {
            default_www_root_path()
        } else {
            let p = std::path::PathBuf::from(cfg);
            if p.is_absolute() {
                p
            } else {
                default_www_root_path().join(cfg)
            }
        };
        Some(p)
    } else {
        None
    };

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
        let rtmp_hook = hook.clone();
        let rtmp_cert = config.rtmp.ssl_cert.clone();
        let rtmp_key = config.rtmp.ssl_key.clone();
        let handle = tokio::spawn(async move {
            match RtmpServer::new(&addr, sm, eb, rtmp_auth, rtmp_hook, rtmp_cert, rtmp_key).await {
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
        let rtsp_hook = hook.clone();
        let handle = tokio::spawn(async move {
            match RtspServer::new(&addr, sm, eb, rtsp_auth, Some(rtsp_hook), rtsp_cert, rtsp_key).await {
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

    // GB28181 SIP + RTP/PS receive server.
    let mut gb28181_rtp: Option<Arc<RtpServerManager>> = None;
    let mut gb28181_sip: Option<Arc<SipServer>> = None;
    if config.gb28181.enabled {
        match Gb28181Server::start(&config.gb28181, source_manager.clone()).await {
            Ok(srv) => {
                gb28181_rtp = Some(srv.rtp.clone());
                gb28181_sip = Some(srv.sip.clone());
                info!(
                    "GB28181 SIP server started on port {}",
                    config.gb28181.sip_port
                );
            }
            Err(e) => error!("Failed to start GB28181 server: {}", e),
        }
    }

    // Real-time transcoding manager (addTranscode API). Always available; the
    // HTTP API returns an error if a caller asks for it but ffmpeg is missing.
    let transcode_mgr: Option<Arc<zlmediakit_transcode::TranscodeManager>> = Some(Arc::new(
        zlmediakit_transcode::TranscodeManager::new(source_manager.clone()),
    ));

    // Global session registry, shared so the kick_session API can terminate
    // any live connection regardless of which HTTP/API server accepted it.
    let session_mgr: Option<Arc<zlmediakit_core::session::SessionManager>> =
        Some(Arc::new(zlmediakit_core::session::SessionManager::new()));

    if config.http.enabled {
        let addr = format!("0.0.0.0:{}", config.http_port);
        let sm = source_manager.clone();
        let http_auth = auth.clone();
        let rec = recorder_control.clone();
        let proxy = proxy_control.clone();
        let pusher = pusher_control.clone();
        let ffmpeg = ffmpeg_control.clone();
        let http_cert = ssl_cert.clone();
        let http_key = ssl_key.clone();
        let http_www = www_root.clone();
        let http_hook = hook.clone();
        let http_rtp = gb28181_rtp.clone();
        let http_sip = gb28181_sip.clone();
        let http_transcode = transcode_mgr.clone();
        let http_session_mgr = session_mgr.clone();
        let handle = tokio::spawn(async move {
            match HttpServer::new(HttpServerConfig {
                addr: addr.clone(),
                source_manager: sm,
                auth: http_auth,
                hook: http_hook,
                recorder: rec,
                proxy,
                pusher,
                ffmpeg,
                rtp: http_rtp.clone(),
                sip: http_sip.clone(),
                transcode: http_transcode.clone(),
                session_manager: http_session_mgr.clone(),
                record_root: std::path::PathBuf::from(&http_record_root),
                www_root: http_www,
                ssl_cert: http_cert,
                ssl_key: http_key,
            })
            .await
            {
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
        let pusher = pusher_control.clone();
        let ffmpeg = ffmpeg_control.clone();
        let api_cert = ssl_cert.clone();
        let api_key = ssl_key.clone();
        let api_www = www_root.clone();
        let api_hook = hook.clone();
        let api_rtp = gb28181_rtp.clone();
        let api_sip = gb28181_sip.clone();
        let api_transcode = transcode_mgr.clone();
        let api_session_mgr = session_mgr.clone();
        let handle = tokio::spawn(async move {
            match HttpServer::new(HttpServerConfig {
                addr: addr.clone(),
                source_manager: sm,
                auth: api_auth,
                hook: api_hook,
                recorder: rec,
                proxy,
                pusher,
                ffmpeg,
                rtp: api_rtp.clone(),
                sip: api_sip.clone(),
                transcode: api_transcode.clone(),
                session_manager: api_session_mgr.clone(),
                record_root: std::path::PathBuf::from(&api_record_root),
                www_root: api_www,
                ssl_cert: api_cert,
                ssl_key: api_key,
            })
            .await
            {
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

    // SRT ingest server: accepts SRT connections and publishes the received
    // stream to the MediaSourceManager. Runs in a blocking thread since
    // the SRT API is synchronous.
    let mut stop_signals: Vec<Arc<AtomicBool>> = Vec::new();

    if config.srt.enabled {
        let srt_addr = format!("0.0.0.0:{}", config.srt.port);
        let srt_sm = source_manager.clone();
        let srt_stop = Arc::new(AtomicBool::new(false));
        stop_signals.push(srt_stop.clone());
        let srt_latency = config.srt.latency_ms;
        let srt_passphrase = config.srt.passphrase.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut server = SrtServer::new(SrtServerConfig {
                addr: srt_addr,
                latency_ms: srt_latency,
                passphrase: srt_passphrase,
                source_manager: srt_sm,
            });
            if let Err(e) = server.run_blocking(srt_stop) {
                error!("SRT server error: {}", e);
            }
        });
        handles.push(handle);
    }

    // Recorder supervisor: always runs so the HTTP API can start/stop recording
    // on demand. It also auto-records every published stream when `record.hls`
    // or `record.flv` is enabled in the config.
    {
        let rec_cfg = config.record.clone();
        let sup_event = event_bus.clone();
        let sup_sm = source_manager.clone();
        info!(
            "Recorder supervisor started (auto hls={}, flv={}, mp4={}) -> {}",
            config.record.hls, config.record.flv, config.record.mp4, record_base
        );
        let rec = recorder_control.clone();
        let sup_hook = hook.clone();
        let handle = tokio::spawn(async move {
            run_recorder_supervisor(
                sup_event,
                sup_sm,
                rec_cfg,
                record_base,
                recorder_cmd_rx,
                rec,
                sup_hook,
            )
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

    // Config-driven auto-pull: start proxy entries from config.
    if config.proxy.enabled {
        for entry in &config.proxy.pulls {
            let stream = if entry.stream.is_empty() {
                entry.url.rsplit('/').next().unwrap_or("proxy").to_string()
            } else {
                entry.stream.clone()
            };
            let ok = proxy_control.add(&entry.url, &entry.vhost, &entry.app, &stream);
            if ok {
                info!(
                    "Auto proxy: {} -> {}/{}/{}",
                    entry.url, entry.vhost, entry.app, stream
                );
            } else {
                warn!("Auto proxy: {} already active, skipping", stream);
            }
        }
    }

    // Stream pusher supervisor: drains `addStreamPusher` commands and spawns
    // push tasks. Each task watches for the local stream to become available
    // and pushes it to the configured remote URL.
    {
        let sup_sm = source_manager.clone();
        let sup_eb = event_bus.clone();
        let sup_active = pusher_active.clone();
        info!("Stream pusher supervisor started");
        let handle = tokio::spawn(async move {
            run_pusher_supervisor(pusher_cmd_rx, sup_eb, sup_sm, sup_active).await;
        });
        handles.push(handle);
    }

    // FFmpeg source supervisor: drains `addFFmpegSource` commands and spawns
    // ffmpeg subprocesses that pull from a remote URL and push to localhost.
    {
        let sup_rtmp_port = config.rtmp_port;
        info!("FFmpeg source supervisor started");
        let handle = tokio::spawn(async move {
            run_ffmpeg_supervisor(ffmpeg_cmd_rx, ffmpeg_active, sup_rtmp_port).await;
        });
        handles.push(handle);
    }

    // Cluster push relay: when a stream is published locally, push it to
    // configured remote peers (RTMP push).
    if config.cluster.enabled && !config.cluster.push_to.is_empty() {
        info!(
            "Cluster push relay: {} peer(s) configured",
            config.cluster.push_to.len()
        );
        let cluster_peers = config.cluster.push_to.clone();
        let cluster_sm = source_manager.clone();
        let cluster_eb = event_bus.clone();
        let handle = tokio::spawn(async move {
            run_cluster_push_relay(cluster_eb, cluster_sm, cluster_peers).await;
        });
        handles.push(handle);
    }

    info!("All servers started. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    // Signal blocking servers (e.g. SRT) to stop.
    for sig in &stop_signals {
        sig.store(true, Ordering::Relaxed);
    }

    // Abort async tasks.
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
    hook: Arc<HookClient>,
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
                                spawn_recorders(src, &base_path, hls, flv, mp4, stop, Some(event_bus.clone())).await;
                            }
                        }
                    }
                    Ok(Event::StreamUnPublish { source_id, .. }) => {
                        if let Some(stop) = stops.lock().await.remove(&source_id) {
                            stop.notify_waiters();
                        }
                        control.unmark_recording(&source_id);
                    }
                    Ok(Event::StreamPlay { source_id, .. }) => {
                        // First subscriber joined → stream changed (regist=true).
                        if let Some((v, a, s)) = split_source_id(&source_id) {
                            let count = source_manager
                                .get(&v, &a, &s)
                                .map(|src| src.subscriber_count_blocking())
                                .unwrap_or(1);
                            match hook.on_stream_changed(&v, &a, &s, true, count).await {
                                HookResult::Allow => {}
                                HookResult::Deny(reason) => {
                                    debug!("hook on_stream_changed denied ({}): {}", source_id, reason)
                                }
                            }
                        }
                    }
                    Ok(Event::StreamStop { source_id, .. }) => {
                        // A subscriber left → stream changed (regist=false).
                        if let Some((v, a, s)) = split_source_id(&source_id) {
                            let count = source_manager
                                .get(&v, &a, &s)
                                .map(|src| src.subscriber_count_blocking())
                                .unwrap_or(0);
                            match hook.on_stream_changed(&v, &a, &s, false, count).await {
                                HookResult::Allow => {}
                                HookResult::Deny(reason) => {
                                    debug!("hook on_stream_changed denied ({}): {}", source_id, reason)
                                }
                            }
                        }
                    }
                    Ok(Event::NoReader { source_id }) => {
                        if let Some((v, a, s)) = split_source_id(&source_id) {
                            let count = source_manager
                                .get(&v, &a, &s)
                                .map(|src| src.subscriber_count_blocking())
                                .unwrap_or(0);
                            match hook.on_stream_none_reader(&v, &a, &s, count).await {
                                HookResult::Allow => {}
                                HookResult::Deny(reason) => {
                                    debug!("hook on_stream_none_reader denied ({}): {}", source_id, reason)
                                }
                            }
                        }
                    }
                    Ok(Event::RecordMp4Done {
                        vhost,
                        app,
                        stream,
                        file_path,
                        file_size,
                        time_len,
                    }) => {
                        if let HookResult::Deny(reason) = hook
                            .on_record_mp4(
                                &vhost,
                                &app,
                                &stream,
                                &file_path,
                                file_size,
                                time_len,
                            )
                            .await
                        {
                            debug!(
                                "hook on_record_mp4 denied ({}): {}",
                                file_path, reason
                            );
                        }
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
                                    spawn_recorders(src, &base_path, hls, flv, mp4, stop, Some(event_bus.clone())).await;
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

/// Splits a `MediaSource` id of the form `vhost/app/stream` into its parts.
/// Returns `None` if the id does not contain at least two `/` separators.
/// Resolves the bundled `www/` directory shipped with the source tree.
///
/// The server binary lives in `crates/server`, so the web UI is two levels up
/// at `<repo>/www`. Using `CARGO_MANIFEST_DIR` makes `cargo run` and tests
/// serve the management UI without any config tweaking.
fn default_www_root_path() -> std::path::PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(manifest).join("../../www")
}

fn split_source_id(id: &str) -> Option<(String, String, String)> {
    let mut it = id.splitn(3, '/');
    let vhost = it.next()?;
    let app = it.next()?;
    let stream = it.next()?;
    if vhost.is_empty() || app.is_empty() || stream.is_empty() {
        return None;
    }
    Some((vhost.to_string(), app.to_string(), stream.to_string()))
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
    event_bus: Option<Arc<EventBus>>,
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
        let eb = event_bus.clone();
        tokio::spawn(async move {
            if let Err(e) = Mp4Recorder::record(src, &path, s, eb).await {
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
            if url.starts_with("rtmp://") || url.starts_with("rtmps://") {
                if let Err(e) =
                    rtmp_pull_client::start(&url, &vhost, &app, &stream, sm, stop, stopped).await
                {
                    warn!("RTMP proxy task error {}: {}", key, e);
                }
            } else {
                if let Err(e) =
                    rtsp_pull_start(&url, &vhost, &app, &stream, sm, stop, stopped).await
                {
                    warn!("stream proxy task error {}: {}", key, e);
                }
            }
            info!("stream proxy task end: {}", key);
            active.remove(&key);
        });
    }
}

/// Drains `PusherCmd` commands from the HTTP API and watches the EventBus
/// for stream publish events. When a local stream matching a pusher request
/// becomes available, it spawns an RTMP push task to the remote URL.
async fn run_pusher_supervisor(
    mut cmd_rx: mpsc::UnboundedReceiver<PusherCmd>,
    event_bus: Arc<EventBus>,
    source_manager: Arc<MediaSourceManager>,
    active: PusherTable,
) {
    use std::collections::HashMap;
    use tokio::sync::Notify;

    let mut event_rx = event_bus.subscribe();
    // Track pending pushers that haven't matched a stream yet.
    // Keyed by (vhost,app,stream), value = (dst_url, stop, stopped).
    let mut pending: HashMap<String, (String, Arc<Notify>, Arc<AtomicBool>)> = HashMap::new();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(c) => {
                        let key = format!("{}/{}/{}", c.vhost, c.app, c.stream);
                        // Check if the stream is already live.
                        if source_manager.get(&c.vhost, &c.app, &c.stream).is_some() {
                            spawn_push_task(c, source_manager.clone(), active.clone());
                        } else {
                            pending.insert(key, (c.dst_url, c.stop, c.stopped));
                        }
                    }
                    None => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(Event::StreamPublish { vhost, app, stream, .. }) => {
                        let key = format!("{}/{}/{}", vhost, app, stream);
                        if let Some((dst_url, stop, stopped)) = pending.remove(&key) {
                            let cmd = PusherCmd {
                                dst_url,
                                vhost,
                                app,
                                stream,
                                stop,
                                stopped,
                            };
                            spawn_push_task(cmd, source_manager.clone(), active.clone());
                        }
                    }
                    Ok(Event::StreamUnPublish { .. }) => {}
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

fn spawn_push_task(cmd: PusherCmd, source_manager: Arc<MediaSourceManager>, active: PusherTable) {
    let key = format!("{}/{}/{}", cmd.vhost, cmd.app, cmd.stream);
    tokio::spawn(async move {
        info!(
            "Stream pusher: {}/{}/{} -> {}",
            cmd.vhost, cmd.app, cmd.stream, cmd.dst_url
        );
        if let Err(e) = rtmp_push_client::start(
            &cmd.dst_url,
            &cmd.vhost,
            &cmd.app,
            &cmd.stream,
            source_manager,
            cmd.stop,
            cmd.stopped,
        )
        .await
        {
            warn!("Stream pusher error {}/{}: {}", cmd.vhost, cmd.app, e);
        }
        info!("Stream pusher ended: {}", key);
        active.remove(&key);
    });
}

/// Drains `FFmpegCmd` commands from the HTTP API and spawns ffmpeg
/// subprocesses that pull from a remote source URL and push to the local
/// RTMP server. Each process is tracked by its (vhost,app,stream) key for
/// later cleanup via `delFFmpegSource`.
async fn run_ffmpeg_supervisor(
    mut cmd_rx: mpsc::UnboundedReceiver<FFmpegCmd>,
    active: FFmpegTable,
    rtmp_port: u16,
) {
    use std::collections::HashMap;
    use tokio::process::Command;

    let mut children: HashMap<String, tokio::process::Child> = HashMap::new();

    while let Some(cmd) = cmd_rx.recv().await {
        let key = format!("{}/{}/{}", cmd.vhost, cmd.app, cmd.stream);

        // If the dst_url wasn't provided, construct one pointing at localhost.
        let dst_url = if cmd.dst_url.is_empty() {
            format!("rtmp://127.0.0.1:{}/{}/{}", rtmp_port, cmd.app, cmd.stream)
        } else {
            cmd.dst_url.clone()
        };

        match Command::new("ffmpeg")
            .args([
                "-i",
                &cmd.src_url,
                "-c",
                "copy",
                "-f",
                "flv",
                "-timeout",
                &cmd.timeout_ms.to_string(),
                &dst_url,
            ])
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => {
                info!(
                    "FFmpeg source started: {} -> {} (pid={:?})",
                    cmd.src_url,
                    dst_url,
                    child.id()
                );
                children.insert(key, child);
            }
            Err(e) => {
                warn!("Failed to spawn ffmpeg for {}: {}", cmd.src_url, e);
                active.remove(&key);
            }
        }

        // Clean up any finished children.
        let mut to_remove = Vec::new();
        for (k, child) in children.iter_mut() {
            if child.try_wait().ok().flatten().is_some() {
                to_remove.push(k.clone());
            }
        }
        for k in to_remove {
            info!("FFmpeg source process ended: {}", k);
            active.remove(&k);
            children.remove(&k);
        }
    }
}

/// Watches the EventBus for `StreamPublish` events and pushes the stream
/// to configured cluster peers via RTMP push.
async fn run_cluster_push_relay(
    event_bus: Arc<EventBus>,
    source_manager: Arc<MediaSourceManager>,
    peers: Vec<zlmediakit_core::config::ClusterPeerConfig>,
) {
    let mut rx = event_bus.subscribe();
    info!("Cluster push relay supervisor started");

    loop {
        match rx.recv().await {
            Ok(Event::StreamPublish {
                vhost, app, stream, ..
            }) => {
                for peer in &peers {
                    let _peer_vhost = if peer.vhost.is_empty() {
                        &vhost
                    } else {
                        &peer.vhost
                    };
                    let peer_app = if peer.app.is_empty() {
                        "live"
                    } else {
                        &peer.app
                    };
                    let push_url =
                        format!("{}/{}/{}", peer.url.trim_end_matches('/'), peer_app, stream);

                    let sm = source_manager.clone();
                    let src_vhost = vhost.clone();
                    let src_app = app.clone();
                    let src_stream = stream.clone();

                    tokio::spawn(async move {
                        info!("Cluster push: {}/{} -> {}", src_app, src_stream, push_url);
                        let stop = Arc::new(tokio::sync::Notify::new());
                        let stopped = Arc::new(AtomicBool::new(false));
                        if let Err(e) = rtmp_push_client::start(
                            &push_url,
                            &src_vhost,
                            &src_app,
                            &src_stream,
                            sm,
                            stop,
                            stopped,
                        )
                        .await
                        {
                            warn!(
                                "Cluster push {}/{} -> {} failed: {}",
                                src_app, src_stream, push_url, e
                            );
                        }
                    });
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
            _ => {}
        }
    }
}
