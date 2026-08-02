use bytes::BytesMut;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{debug, error, warn};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::ffmpeg_source::FFmpegSourceControl;
use zlmediakit_core::hook::HookResult;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::rtp::{RtpDataType, RtpSendRequest, RtpSenderManager};
use zlmediakit_core::session::SessionManager;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_core::stream_pusher::StreamPusherControl;
use zlmediakit_core::transport::TransportStream;
use zlmediakit_hls::muxer::{self, HlsSegment};
use zlmediakit_hls::TsLiveMuxer;
use zlmediakit_srt::{RtpPayloadType, RtpServerManager, RtpTcpMode, SipServer};
use zlmediakit_transcode::{TranscodeConfig, TranscodeManager};

use crate::ws::{upgrade_response, WsSession};
use zlmediakit_flv::FlvMuxer;
use zlmediakit_mp4::fmp4::Fmp4Muxer;
use zlmediakit_mp4::{
    build_mpd, DashRepresentation, Fmp4Segment, Mp4VodLibrary, TrackKind, VodPacer, VOD_APP,
};

pub static HLS_SEGMENTS: once_cell::sync::Lazy<DashMap<String, Arc<RwLock<VecDeque<HlsSegment>>>>> =
    once_cell::sync::Lazy::new(DashMap::new);

/// DASH fMP4 segment cache. Key = stream URL (vhost/app/stream).
pub static DASH_SEGMENTS: once_cell::sync::Lazy<
    DashMap<String, Arc<RwLock<VecDeque<Fmp4Segment>>>>,
> = once_cell::sync::Lazy::new(DashMap::new);

/// CMAF HLS segment cache (fMP4-based). Key = stream URL.
pub static CMAF_SEGMENTS: once_cell::sync::Lazy<
    DashMap<String, Arc<RwLock<VecDeque<zlmediakit_hls::HlsSegment>>>>,
> = once_cell::sync::Lazy::new(DashMap::new);

/// CMAF init segment cache. Key = stream URL.
type CmafInitCache = DashMap<String, Arc<RwLock<Option<Vec<u8>>>>>;
pub static CMAF_INIT_CACHE: once_cell::sync::Lazy<CmafInitCache> =
    once_cell::sync::Lazy::new(DashMap::new);

pub struct HttpSession {
    stream: TransportStream,
    peer_addr: String,
    source_manager: Arc<MediaSourceManager>,
    rtp_sender: Arc<RtpSenderManager>,
    auth: Arc<StreamAuth>,
    hook: Arc<zlmediakit_core::hook::HookClient>,
    recorder: Arc<RecorderControl>,
    proxy: Arc<StreamProxyControl>,
    pusher: Arc<StreamPusherControl>,
    ffmpeg: Arc<FFmpegSourceControl>,
    rtp: Option<Arc<RtpServerManager>>,
    sip: Option<Arc<SipServer>>,
    transcode: Option<Arc<TranscodeManager>>,
    session_manager: Option<Arc<SessionManager>>,
    record_root: PathBuf,
    www_root: Option<PathBuf>,
    buffer: BytesMut,
}

impl HttpSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: TransportStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
        rtp_sender: Arc<RtpSenderManager>,
        auth: Arc<StreamAuth>,
        hook: Arc<zlmediakit_core::hook::HookClient>,
        recorder: Arc<RecorderControl>,
        proxy: Arc<StreamProxyControl>,
        pusher: Arc<StreamPusherControl>,
        ffmpeg: Arc<FFmpegSourceControl>,
        rtp: Option<Arc<RtpServerManager>>,
        sip: Option<Arc<SipServer>>,
        transcode: Option<Arc<TranscodeManager>>,
        session_manager: Option<Arc<SessionManager>>,
        record_root: PathBuf,
        www_root: Option<PathBuf>,
    ) -> Self {
        Self {
            stream,
            peer_addr,
            source_manager,
            rtp_sender,
            auth,
            hook,
            recorder,
            proxy,
            pusher,
            ffmpeg,
            rtp,
            sip,
            transcode,
            session_manager,
            record_root,
            www_root,
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        // Register this connection in the global session registry (if any)
        // so the kick_session API can terminate it. The guard removes the
        // record when `run` returns.
        struct SessionGuard {
            mgr: Option<Arc<SessionManager>>,
            id: Option<String>,
        }
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                if let (Some(mgr), Some(id)) = (&self.mgr, &self.id) {
                    mgr.remove(id);
                }
            }
        }
        let _guard = self.session_manager.as_ref().map(|m| {
            let local_addr = self
                .stream
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let id = m.create_with_local(self.peer_addr.clone(), local_addr, "HTTP".to_string());
            SessionGuard {
                mgr: self.session_manager.clone(),
                id: Some(id),
            }
        });

        let mut read_buf = [0u8; 4096];
        match self.stream.read(&mut read_buf).await {
            Ok(0) => return Ok(()),
            Ok(n) => {
                self.buffer.extend_from_slice(&read_buf[..n]);
            }
            Err(e) => {
                error!("HTTP read error: {}", e);
                return Ok(());
            }
        }

        let request_str = String::from_utf8_lossy(&self.buffer).to_string();
        let first_line = request_str.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.splitn(3, ' ').collect();

        if parts.len() < 2 {
            self.send_400().await?;
            return Ok(());
        }

        let _method = parts[0];
        let path = parts[1];

        debug!("HTTP {} {} from {}", _method, path, self.peer_addr);

        // WebSocket upgrade detection
        let is_ws = request_str.contains("Upgrade: websocket")
            || request_str.contains("upgrade: websocket");
        if is_ws {
            let ws_key = request_str
                .lines()
                .find(|l| l.to_lowercase().starts_with("sec-websocket-key:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim())
                .unwrap_or("");

            if !ws_key.is_empty() {
                let response = upgrade_response(ws_key);
                self.stream.write_all(response.as_bytes()).await?;
                let mut ws = WsSession::new(
                    self.stream,
                    self.peer_addr.clone(),
                    self.source_manager,
                    self.auth,
                    self.hook.clone(),
                )
                .with_vod_library(Arc::new(Mp4VodLibrary::new(self.record_root)));
                let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
                if route.ends_with(".live.ts") {
                    return ws.run_ts(path).await;
                }
                if route.ends_with(".live.mp4") {
                    return ws.run_fmp4(path).await;
                }
                return ws.run_flv(path).await;
            }
        }

        // Route on the path without any query string; the handlers still
        // receive the full `path` so they can extract `?sign=` and other params.
        let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);

        // Unified HTTP access-control hook (`on_http_access`). Applied to all
        // stream-playback routes so an external service can deny playback
        // (and other requests) before any stream/auth logic runs. Fail-open:
        // when no hook is configured this is a no-op.
        if Self::is_play_route(route) {
            let (app, resource, params) = Self::parse_path_sign(path);
            let stream_name = resource.to_string();
            let peer_ip = self.peer_addr.split(':').next().unwrap_or(&self.peer_addr);
            if let HookResult::Deny(msg) = self
                .hook
                .on_http_access(zlmediakit_core::hook::HttpAccessParams {
                    vhost: "__defaultVhost__",
                    app: &app,
                    stream: &stream_name,
                    ip: peer_ip,
                    params: &params,
                    path: route,
                    is_play: true,
                    url: path,
                })
                .await
            {
                warn!(
                    "HTTP access denied by hook (on_http_access): {}/{} - {}",
                    app, stream_name, msg
                );
                return self.send_401().await;
            }
        }

        let www_root = self.www_root.clone();
        if route.starts_with("/index/api/") {
            if !self.is_api_authorized(path, &request_str) {
                warn!("HTTP API authentication failed from {}", self.peer_addr);
                self.send_api_unauthorized().await?;
                return Ok(());
            }
            self.handle_api(path).await?;
        } else if route.starts_with("/record/") {
            self.handle_vod(path, &request_str).await?;
        } else if route.ends_with(".flv") {
            self.handle_flv_stream(path).await?;
        } else if route.ends_with(".live.ts") {
            self.handle_ts_stream(path).await?;
        } else if route.ends_with(".live.mp4") {
            self.handle_fmp4_stream(path).await?;
        } else if route.ends_with(".mpd") {
            self.handle_dash_mpd(path).await?;
        } else if route.ends_with(".m4s") {
            // Try CMAF first (seg-{idx}.m4s), fall back to DASH (seg-{rep}-{seq}.m4s)
            if !self.handle_cmaf_segment(path).await? {
                self.handle_dash_segment(path).await?;
            }
        } else if route.ends_with("init.mp4") {
            self.handle_dash_init(path).await?;
        } else if route.ends_with(".cmav.m3u8") {
            self.handle_cmaf_playlist(path).await?;
        } else if route.ends_with(".m3u8") {
            self.handle_hls_playlist(path).await?;
        } else if route.ends_with(".ts") {
            self.handle_hls_segment(path).await?;
        } else if route == "/" || route == "/index.html" {
            if let Some(ref www) = www_root {
                // When a web root is configured, serve from it instead of the default index
                self.handle_static_file(path, www, &request_str).await?;
            } else {
                self.handle_index().await?;
            }
        } else if let Some(ref www) = www_root {
            // Static file fallback: serve from www_root
            self.handle_static_file(path, www, &request_str).await?;
        } else {
            self.send_404().await?;
        }

        Ok(())
    }

    async fn handle_flv_stream(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".flv")
            .map_or("stream", |v| v)
            .to_string();

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "HTTP-FLV play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HTTP-FLV play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;

        match source {
            Some(source) => {
                let _subscriber = source
                    .register_subscriber(format!("http-flv:{}", self.peer_addr))
                    .await;
                let response = "HTTP/1.1 200 OK\r\n\
                    Content-Type: video/x-flv\r\n\
                    Connection: close\r\n\
                    Cache-Control: no-cache\r\n\r\n";
                self.stream.write_all(response.as_bytes()).await?;

                let mut muxer = FlvMuxer::new();

                let header = muxer.write_header();
                self.stream.write_all(&header).await?;

                let info = source.info.read().await;
                if let Some(zlmediakit_core::media_frame::TrackInfo::Video(ref v)) =
                    info.tracks.first()
                {
                    let meta = muxer.write_metadata(v.width, v.height, v.fps, 44100, v.codec);
                    self.stream.write_all(&meta).await?;
                }
                drop(info);

                let cached = {
                    let cache = source.gop_cache.read().await;
                    cache.get_latest_gop_frames()
                };

                for frame in &cached {
                    let tag = muxer.write_tag(frame)?;
                    self.stream.write_all(&tag).await?;
                }

                let mut rx = source.subscribe();

                while let Ok(frame) = rx.recv().await {
                    if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                        continue;
                    }
                    let tag = muxer.write_tag(&frame)?;
                    self.stream.write_all(&tag).await?;
                }
            }
            None => {
                self.send_404().await?;
            }
        }

        Ok(())
    }

    /// Serves a continuous MPEG-TS stream (`/app/stream.live.ts`).
    ///
    /// Mirrors `handle_flv_stream`: config frames and the latest GOP are
    /// replayed first so the player can decode immediately, then every live
    /// frame is muxed to TS on the fly.
    async fn handle_ts_stream(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".live.ts")
            .map_or("stream", |v| v)
            .to_string();

        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "HTTP-TS play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HTTP-TS play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;

        match source {
            Some(source) => {
                let _subscriber = source
                    .register_subscriber(format!("http-ts:{}", self.peer_addr))
                    .await;
                let response = "HTTP/1.1 200 OK\r\n\
                    Content-Type: video/MP2T\r\n\
                    Connection: close\r\n\
                    Cache-Control: no-cache\r\n\r\n";
                self.stream.write_all(response.as_bytes()).await?;

                let mut muxer = TsLiveMuxer::new();

                let configs = {
                    let cache = source.gop_cache.read().await;
                    cache.get_config_frames()
                };
                for frame in &configs {
                    let ts = muxer.push_frame(frame);
                    if !ts.is_empty() {
                        self.stream.write_all(&ts).await?;
                    }
                }

                let cached = {
                    let cache = source.gop_cache.read().await;
                    cache.get_latest_gop_frames()
                };
                for frame in &cached {
                    let ts = muxer.push_frame(frame);
                    if !ts.is_empty() {
                        self.stream.write_all(&ts).await?;
                    }
                }

                let mut rx = source.subscribe();
                while let Ok(frame) = rx.recv().await {
                    let ts = muxer.push_frame(&frame);
                    if !ts.is_empty() {
                        if let Err(e) = self.stream.write_all(&ts).await {
                            debug!("HTTP-TS write error: {}", e);
                            break;
                        }
                    }
                }
            }
            None => {
                self.send_404().await?;
            }
        }

        Ok(())
    }

    /// Serves a live fragmented-MP4 stream (`/app/stream.live.mp4`).
    ///
    /// The init segment (ftyp+moov) is written once, then a media segment
    /// (styp+moof+mdat) is written on every keyframe, giving GOP-aligned
    /// fragments a player can decode progressively.
    async fn handle_fmp4_stream(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".live.mp4")
            .map_or("stream", |v| v)
            .to_string();

        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "HTTP-fMP4 play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HTTP-fMP4 play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;

        match source {
            Some(source) => {
                let _subscriber = source
                    .register_subscriber(format!("http-fmp4:{}", self.peer_addr))
                    .await;
                let response = "HTTP/1.1 200 OK\r\n\
                    Content-Type: video/mp4\r\n\
                    Connection: close\r\n\
                    Cache-Control: no-cache\r\n\r\n";
                self.stream.write_all(response.as_bytes()).await?;

                let mut muxer = Fmp4Muxer::new(90_000);

                let configs = {
                    let cache = source.gop_cache.read().await;
                    cache.get_config_frames()
                };
                for frame in &configs {
                    muxer.push_frame(frame);
                }
                let init = muxer.init_segment();
                self.stream.write_all(&init.data).await?;

                let cached = {
                    let cache = source.gop_cache.read().await;
                    cache.get_latest_gop_frames()
                };
                for frame in &cached {
                    if zlmediakit_core::gop_cache::is_config_frame(frame) {
                        continue;
                    }
                    muxer.push_frame(frame);
                    if frame.frame_type == zlmediakit_core::media_frame::FrameType::Video
                        && frame.key_frame
                    {
                        if let Some(seg) = muxer.flush_segment() {
                            if let Err(e) = self.stream.write_all(&seg.data).await {
                                debug!("HTTP-fMP4 write error: {}", e);
                                return Ok(());
                            }
                        }
                    }
                }
                if let Some(seg) = muxer.flush_segment() {
                    if let Err(e) = self.stream.write_all(&seg.data).await {
                        debug!("HTTP-fMP4 write error: {}", e);
                        return Ok(());
                    }
                }

                let mut rx = source.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                                continue;
                            }
                            muxer.push_frame(&frame);
                            if frame.frame_type == zlmediakit_core::media_frame::FrameType::Video
                                && frame.key_frame
                            {
                                if let Some(seg) = muxer.flush_segment() {
                                    if let Err(e) = self.stream.write_all(&seg.data).await {
                                        debug!("HTTP-fMP4 write error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
            None => {
                self.send_404().await?;
            }
        }

        Ok(())
    }

    async fn handle_hls_playlist(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        // Supported forms: `/app/stream.m3u8` and `/app/stream/hls.m3u8`;
        // `resource` is everything after the app segment.
        let stream_name = if let Some(s) = resource.strip_suffix("/hls.m3u8") {
            s
        } else if let Some(s) = resource.strip_suffix(".m3u8") {
            s
        } else {
            &resource
        }
        .to_string();

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "HLS playlist play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HLS playlist play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;

        if let Some(source) = source {
            let key = source.url();
            let segments = HLS_SEGMENTS.entry(key.clone()).or_insert_with(|| {
                let segs: Arc<RwLock<VecDeque<HlsSegment>>> =
                    Arc::new(RwLock::new(VecDeque::new()));
                let segs_clone = segs.clone();
                let source_clone = source.clone();
                let cleanup_key = key.clone();
                muxer::start_hls_task(
                    source_clone,
                    segs_clone,
                    4.0,
                    10,
                    Some(Box::new(move || {
                        HLS_SEGMENTS.remove(&cleanup_key);
                    })),
                );
                segs
            });

            let segs_guard = segments.read().await;
            let count = segs_guard.len();
            let first_idx = segs_guard.front().map(|s| s.index).unwrap_or(0);
            drop(segs_guard);

            let mut m3u8 = String::new();
            m3u8.push_str("#EXTM3U\n");
            m3u8.push_str("#EXT-X-VERSION:3\n");
            m3u8.push_str("#EXT-X-TARGETDURATION:4\n");

            if count > 0 {
                m3u8.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", first_idx));
                let segs_guard = segments.read().await;
                for seg in segs_guard.iter() {
                    m3u8.push_str(&format!("#EXTINF:{:.3},\n", seg.duration));
                    m3u8.push_str(&format!("/{}/{}/{}.ts\n", app, stream_name, seg.index));
                }
            } else {
                m3u8.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
            }

            let body = m3u8;
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/vnd.apple.mpegurl\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n{}",
                body.len(),
                body
            );
            self.stream.write_all(response.as_bytes()).await?;
        } else {
            self.send_404().await?;
        }

        Ok(())
    }

    async fn handle_hls_segment(&mut self, path: &str) -> anyhow::Result<()> {
        // Path form: `/app/stream/<idx>.ts`; `resource` = "stream/<idx>.ts".
        let (app, resource, sign) = Self::parse_path_sign(path);
        let parts: Vec<&str> = resource.split('/').collect();

        if parts.len() < 2 {
            self.send_404().await?;
            return Ok(());
        }

        let app = app.as_str();
        let stream_name = parts[0];
        let file_name = parts[1];

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", app, stream_name, &sign)
            .await
        {
            warn!(
                "HLS segment play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", app, stream_name, "play", &sign)
        {
            warn!("HLS segment play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let idx: u32 = if let Some(name) = file_name.strip_suffix(".ts") {
            name.parse().unwrap_or(u32::MAX)
        } else {
            u32::MAX
        };

        if idx == u32::MAX {
            self.send_404().await?;
            return Ok(());
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", app, stream_name);

        if let Some(source) = source {
            let key = source.url();
            if let Some(segments) = HLS_SEGMENTS.get(&key) {
                let segs = segments.read().await;
                if let Some(seg) = segs.iter().find(|s| s.index == idx) {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: video/mp2t\r\n\
                         Content-Length: {}\r\n\
                         Cache-Control: max-age=3600\r\n\
                         Connection: close\r\n\r\n",
                        seg.data.len()
                    );
                    self.stream.write_all(response.as_bytes()).await?;
                    self.stream.write_all(&seg.data).await?;
                    return Ok(());
                }
            }
        }

        self.send_404().await
    }

    // ── CMAF HLS handlers ──────────────────────────────────────────────

    /// Serves a CMAF-compatible HLS playlist (.cmav.m3u8).
    async fn handle_cmaf_playlist(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".cmav.m3u8")
            .and_then(|s| s.strip_suffix("/hls"))
            .unwrap_or_else(|| resource.strip_suffix(".cmav.m3u8").unwrap_or("stream"))
            .to_string();

        if let HookResult::Deny(_) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;
        let source = match source {
            Some(s) => s,
            None => return self.send_404().await,
        };

        let key = source.url();

        // Ensure CMAF task is running
        let _segs = CMAF_SEGMENTS.entry(key.clone()).or_insert_with(|| {
            let segs: Arc<RwLock<VecDeque<zlmediakit_hls::HlsSegment>>> =
                Arc::new(RwLock::new(VecDeque::new()));
            let init: Arc<RwLock<Option<Vec<u8>>>> = Arc::new(RwLock::new(None));
            CMAF_INIT_CACHE.insert(key.clone(), init.clone());

            let segs_clone = segs.clone();
            let src = source.clone();
            let init_clone = init.clone();
            let cleanup_key = key.clone();
            zlmediakit_hls::cmaf::start_hls_cmaf_task(
                src,
                segs_clone,
                init_clone,
                Some(Box::new(move || {
                    CMAF_SEGMENTS.remove(&cleanup_key);
                    CMAF_INIT_CACHE.remove(&cleanup_key);
                })),
            );
            segs
        });

        let segs_guard = _segs.read().await;
        let m3u8 = zlmediakit_hls::cmaf::build_cmaf_playlist(&segs_guard, 4, &app, &stream_name);

        let body = m3u8;
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/vnd.apple.mpegurl\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    // ── DASH handlers ──────────────────────────────────────────────────

    /// Serves DASH MPD manifest for a live stream.
    async fn handle_dash_mpd(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".mpd")
            .map_or("stream", |v| v)
            .to_string();

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &app, &stream_name)
            .await;
        let source = match source {
            Some(s) => s,
            None => return self.send_404().await,
        };

        let info = source.info.read().await;
        let mut representations = Vec::new();

        for track in &info.tracks {
            match track {
                zlmediakit_core::media_frame::TrackInfo::Video(v) => {
                    let codec_str = match v.codec {
                        zlmediakit_core::media_frame::CodecId::H264 => "avc1.64001f",
                        zlmediakit_core::media_frame::CodecId::H265 => "hev1.1.6.L93.B0",
                        zlmediakit_core::media_frame::CodecId::VP8 => "vp8",
                        zlmediakit_core::media_frame::CodecId::VP9 => "vp09.00.10.08",
                        zlmediakit_core::media_frame::CodecId::AV1 => "av01.0.04M.08",
                        _ => "avc1.64001f",
                    };
                    representations.push(DashRepresentation {
                        id: "v0".into(),
                        mime_type: "video/mp4".into(),
                        codecs: codec_str.into(),
                        width: v.width,
                        height: v.height,
                        segment_duration_secs: 4,
                        timescale: 90000,
                        bandwidth: 2000000,
                    });
                }
                zlmediakit_core::media_frame::TrackInfo::Audio(a) => {
                    let codec_str = match a.codec {
                        zlmediakit_core::media_frame::CodecId::AAC => "mp4a.40.2",
                        zlmediakit_core::media_frame::CodecId::Opus => "opus",
                        _ => "mp4a.40.2",
                    };
                    representations.push(DashRepresentation {
                        id: "a0".into(),
                        mime_type: "audio/mp4".into(),
                        codecs: codec_str.into(),
                        width: 0,
                        height: 0,
                        segment_duration_secs: 4,
                        timescale: a.sample_rate,
                        bandwidth: 128000,
                    });
                }
            }
        }
        drop(info);

        let base_url = format!("/{}/{}", app, stream_name);
        let mpd = build_mpd(
            &representations,
            &format!("{}/init-{}", base_url, "$RepresentationID$"),
            &format!("{}/seg-{}-$", base_url, "$RepresentationID$"),
            1,
        );

        let body = mpd;
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/dash+xml\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    /// Serves a DASH/CMAF init segment.
    async fn handle_dash_init(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);

        // Check CMAF init cache first
        let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
        // Path: /app/stream/init.mp4 (CMAF) or /app/stream/init-v0.mp4 (DASH)
        if route.ends_with("init.mp4") && !route.contains("-v") && !route.contains("-a") {
            let stream_name = route
                .trim_start_matches('/')
                .split('/')
                .nth(1)
                .unwrap_or("stream");
            if let Some(source) = self
                .source_manager
                .get("__defaultVhost__", &app, stream_name)
            {
                if let Some(init_cache) = CMAF_INIT_CACHE.get(&source.url()) {
                    let guard = init_cache.read().await;
                    if let Some(ref data) = *guard {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: video/mp4\r\n\
                             Content-Length: {}\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Cache-Control: max-age=3600\r\n\
                             Connection: close\r\n\r\n",
                            data.len()
                        );
                        self.stream.write_all(response.as_bytes()).await?;
                        self.stream.write_all(data).await?;
                        return Ok(());
                    }
                }
            }
        }

        // Existing DASH init handler...
        let rest = resource.strip_suffix("init.mp4").unwrap_or(&resource);
        let rest = rest.trim_end_matches('/');
        let stream_name = if let Some(s) = rest.strip_suffix("-v0") {
            s
        } else if let Some(s) = rest.strip_suffix("-a0") {
            s
        } else {
            rest
        };

        if !self
            .auth
            .check("__defaultVhost__", &app, stream_name, "play", &sign)
        {
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, stream_name);
        let source = match source {
            Some(s) => s,
            None => return self.send_404().await,
        };

        // Ensure DASH muxer exists
        let key = source.url();
        let _segs = DASH_SEGMENTS.entry(key.clone()).or_insert_with(|| {
            let segs: Arc<RwLock<VecDeque<Fmp4Segment>>> = Arc::new(RwLock::new(VecDeque::new()));
            let segs_clone = segs.clone();
            let src = source.clone();
            let cleanup_key = key.clone();
            tokio::spawn(async move {
                start_dash_task(src, segs_clone).await;
                DASH_SEGMENTS.remove(&cleanup_key);
            });
            segs
        });

        // Build init segment from current config
        let mut muxer = Fmp4Muxer::new(90000);
        let cached_configs = {
            let cache = source.gop_cache.read().await;
            cache.get_config_frames()
        };
        for frame in &cached_configs {
            muxer.push_frame(frame);
        }
        let init = muxer.init_segment();

        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: video/mp4\r\n\
             Content-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Cache-Control: max-age=3600\r\n\
             Connection: close\r\n\r\n",
            init.data.len()
        );
        self.stream.write_all(response.as_bytes()).await?;
        self.stream.write_all(&init.data).await?;
        Ok(())
    }

    /// Serves a CMAF media segment (seg-{index}.m4s).
    /// Returns `Ok(true)` if the segment was found and served, `Ok(false)` if
    /// the path did not match CMAF format, so the caller can fall back to DASH.
    async fn handle_cmaf_segment(&mut self, path: &str) -> anyhow::Result<bool> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        let rest = match resource.strip_suffix(".m4s") {
            Some(r) => r,
            None => return Ok(false),
        };
        // CMAF path: /app/stream/seg-{index}.m4s
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() < 2 {
            return Ok(false);
        }
        let stream_name = parts[0];
        let segment_part = parts[1];
        if !segment_part.starts_with("seg-") {
            return Ok(false);
        }
        let idx: u64 = segment_part
            .strip_prefix("seg-")
            .and_then(|s| s.parse().ok())
            .unwrap_or(u64::MAX);

        if !self
            .auth
            .check("__defaultVhost__", &app, stream_name, "play", &sign)
        {
            return Ok(false);
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, stream_name);
        let source = match source {
            Some(s) => s,
            None => return Ok(false),
        };

        let key = source.url();
        if let Some(segments) = CMAF_SEGMENTS.get(&key) {
            let segs = segments.read().await;
            if let Some(seg) = segs.iter().find(|s| s.index == idx as u32) {
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: video/mp4\r\n\
                     Content-Length: {}\r\n\
                     Access-Control-Allow-Origin: *\r\n\
                     Cache-Control: max-age=3600\r\n\
                     Connection: close\r\n\r\n",
                    seg.data.len()
                );
                self.stream.write_all(response.as_bytes()).await?;
                self.stream.write_all(&seg.data).await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Serves a DASH media segment (.m4s).
    async fn handle_dash_segment(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = Self::parse_path_sign(path);
        // Path: /app/stream/seg-v0-1.m4s
        let rest = resource.strip_suffix(".m4s").unwrap_or(&resource);
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() < 3 {
            return self.send_404().await;
        }
        let stream_name = parts[0];
        // repr = parts[1], seq = parts[2]
        if parts.len() >= 3 {
            let _seq: u64 = parts.last().unwrap_or(&"0").parse().unwrap_or(0);
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, stream_name, "play", &sign)
        {
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, stream_name);
        let source = match source {
            Some(s) => s,
            None => return self.send_404().await,
        };

        let key = source.url();
        if let Some(segments) = DASH_SEGMENTS.get(&key) {
            let segs = segments.read().await;
            if let Some(seg) = segs.back() {
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: video/mp4\r\n\
                     Content-Length: {}\r\n\
                     Access-Control-Allow-Origin: *\r\n\
                     Cache-Control: max-age=3600\r\n\
                     Connection: close\r\n\r\n",
                    seg.data.len()
                );
                self.stream.write_all(response.as_bytes()).await?;
                self.stream.write_all(&seg.data).await?;
                return Ok(());
            }
        }

        self.send_404().await
    }

    /// Serves recorded files (FLV/MP4/HLS) and a directory listing under the
    /// `/record/` prefix, mapping the remainder of the path onto `record_root`.
    /// Supports HTTP `Range` requests so players can seek within archives.
    /// When auth is enabled, the request must carry a valid `?sign=` parameter.
    async fn handle_vod(&mut self, path: &str, request: &str) -> anyhow::Result<()> {
        let (_, _rest, sign) = Self::parse_path_sign(path);
        // Route is the path without query string
        let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
        let rel = match route.strip_prefix("/record/") {
            Some(r) => r,
            None => return self.send_404().await,
        };

        // Auth check for VOD playback (strip trailing slash for consistent signing)
        let stream_path = rel.trim_end_matches('/').to_string();

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", "record", &stream_path, &sign)
            .await
        {
            warn!("VOD play rejected (hook): {} - {}", rel, msg);
            return self.send_401().await;
        }

        if !self
            .auth
            .check("__defaultVhost__", "record", &stream_path, "play", &sign)
        {
            warn!("VOD play rejected (auth): {}", rel);
            return self.send_401().await;
        }

        // MP4 → FLV remux playback: a request for `<file>.mp4.flv` streams the
        // MP4 file's media (parsed via Mp4Demuxer) as an HTTP-FLV stream so
        // existing FLV players can play MP4 archives.
        if rel.ends_with(".mp4.flv") {
            let mp4_rel = rel.trim_end_matches(".flv");
            let query = Self::parse_query(path);
            let start_ms = query
                .get("start")
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| (value * 1000.0) as u64)
                .unwrap_or(0);
            let duration_ms = query
                .get("duration")
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| (value * 1000.0) as u64);
            return self
                .handle_mp4_vod_flv(mp4_rel, start_ms, duration_ms)
                .await;
        }

        let target = match Self::safe_join(&self.record_root, rel) {
            Some(t) => t,
            None => return self.send_404().await,
        };
        let meta = match tokio::fs::metadata(&target).await {
            Ok(m) => m,
            Err(_) => return self.send_404().await,
        };
        if meta.is_dir() {
            return self.serve_dir_listing(&target, route).await;
        }
        let range = Self::extract_range(request);
        self.serve_file(&target, meta.len(), range).await
    }

    /// Serves an MP4 archive as an HTTP-FLV stream by demuxing it and
    /// remuxing the frames on the fly (`.mp4.flv` URL convention).
    async fn handle_mp4_vod_flv(
        &mut self,
        relative_path: &str,
        start_ms: u64,
        duration_ms: Option<u64>,
    ) -> anyhow::Result<()> {
        let library = Mp4VodLibrary::new(self.record_root.clone());
        let asset = match library.open(VOD_APP, relative_path).await {
            Ok(Some(asset)) => asset,
            Ok(None) => return self.send_404().await,
            Err(e) => {
                warn!("MP4 VOD open failed for {}: {e:#}", relative_path);
                return self.send_404().await;
            }
        };

        let tracks = asset.tracks();
        let playback = asset.playback(start_ms);

        let video = tracks.iter().find(|t| t.handler == TrackKind::Video);
        let audio = tracks.iter().find(|t| t.handler == TrackKind::Audio);
        let width = video.map(|v| v.width).unwrap_or(0);
        let height = video.map(|v| v.height).unwrap_or(0);
        let fps = video.map(|v| v.fps).unwrap_or(0.0);
        let audio_sample_rate = audio.map(|a| a.sample_rate).unwrap_or(0);
        let video_codec = match video {
            Some(v) => v.codec,
            None => {
                return self.send_404().await;
            }
        };

        let response = "HTTP/1.1 200 OK\r\n\
            Content-Type: video/x-flv\r\n\
            Access-Control-Allow-Origin: *\r\n\
            Cache-Control: no-cache\r\n\
            Connection: close\r\n\r\n";
        self.stream.write_all(response.as_bytes()).await?;

        let mut muxer = FlvMuxer::new();
        self.stream.write_all(&muxer.write_header()).await?;
        let meta = muxer.write_metadata(width, height, fps, audio_sample_rate, video_codec);
        self.stream.write_all(&meta).await?;

        let pacer = VodPacer::new();
        for frame in playback.frames {
            if !frame.config_frame {
                if duration_ms.is_some_and(|limit| u64::from(frame.timestamp) > limit) {
                    break;
                }
                pacer.wait(frame.timestamp).await;
            }
            let tag = muxer.write_tag(&frame)?;
            self.stream.write_all(&tag).await?;
        }

        Ok(())
    }

    /// Serves a file or directory from `www_root` for paths that didn't match
    /// any other route. Used as the static file fallback.
    async fn handle_static_file(
        &mut self,
        path: &str,
        www_root: &Path,
        request: &str,
    ) -> anyhow::Result<()> {
        let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
        let rel = route.trim_start_matches('/');
        let target = match Self::safe_join(www_root, rel) {
            Some(t) => t,
            None => return self.send_404().await,
        };
        // Additional safety: ensure resolved path is still under www_root
        if !target.starts_with(www_root) {
            return self.send_404().await;
        }
        let meta = match tokio::fs::metadata(&target).await {
            Ok(m) => m,
            Err(_) => return self.send_404().await,
        };
        if meta.is_dir() {
            let index = target.join("index.html");
            match tokio::fs::metadata(&index).await {
                Ok(idx_meta) => {
                    let range = Self::extract_range(request);
                    return self.serve_file(&index, idx_meta.len(), range).await;
                }
                Err(_) => return self.serve_dir_listing(&target, route).await,
            }
        }
        let range = Self::extract_range(request);
        self.serve_file(&target, meta.len(), range).await
    }

    async fn serve_file(
        &mut self,
        path: &Path,
        total: u64,
        range: Option<(u64, Option<u64>)>,
    ) -> anyhow::Result<()> {
        let (start, end, status_line) = match range {
            Some((s, e)) if total > 0 => {
                let last = total - 1;
                let end = e.unwrap_or(last).min(last);
                let start = s.min(end);
                (start, end, "206 Partial Content")
            }
            _ => (0, total.saturating_sub(1), "200 OK"),
        };
        let len = end - start + 1;
        let mime = Self::mime_for(path);

        let mut header = format!(
            "HTTP/1.1 {}\r\n\
             Content-Type: {}\r\n\
             Accept-Ranges: bytes\r\n\
             Content-Length: {}\r\n",
            status_line, mime, len
        );
        if status_line.starts_with("206") {
            header.push_str(&format!(
                "Content-Range: bytes {}-{}/{}\r\n",
                start, end, total
            ));
        }
        header.push_str("Connection: close\r\n\r\n");
        self.stream.write_all(header.as_bytes()).await?;

        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;

        let mut remaining = len;
        let mut buf = [0u8; 65536];
        while remaining > 0 {
            let to_read = remaining.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..to_read]).await?;
            self.stream.write_all(&buf[..to_read]).await?;
            remaining -= to_read as u64;
        }
        Ok(())
    }

    async fn serve_dir_listing(&mut self, dir: &Path, route: &str) -> anyhow::Result<()> {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(e) => e,
            Err(_) => return self.send_404().await,
        };
        let mut items: Vec<(String, bool)> = Vec::new();
        while let Some(ent) = entries.next_entry().await? {
            let name = ent.file_name().to_string_lossy().to_string();
            let is_dir = ent.metadata().await.map(|m| m.is_dir()).unwrap_or(false);
            items.push((name, is_dir));
        }
        items.sort();

        let base = route.trim_end_matches('/');
        let mut html = String::from(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <title>Recordings</title></head><body><h1>Recordings</h1><ul>",
        );
        for (name, is_dir) in &items {
            let suffix = if *is_dir { "/" } else { "" };
            html.push_str(&format!(
                "<li><a href=\"{}{}{}\">{}{}</a></li>",
                base,
                if base.is_empty() { "" } else { "/" },
                name,
                name,
                suffix
            ));
        }
        html.push_str("</ul></body></html>");

        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            html.len(),
            html
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    /// Joins `rel` onto `root`, rejecting any path that would escape `root`
    /// (via `..` or absolute components). Returns `None` for unsafe paths.
    fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim_start_matches('/');
        let mut out = root.to_path_buf();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(c) => out.push(c),
                Component::CurDir => {}
                Component::ParentDir => return None,
                Component::RootDir => return None,
                Component::Prefix(_) => return None,
            }
        }
        Some(out)
    }

    fn mime_for(path: &Path) -> &'static str {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            // video
            Some("flv") => "video/x-flv",
            Some("mp4") | Some("m4v") | Some("mov") => "video/mp4",
            Some("ts") => "video/mp2t",
            Some("m4s") => "video/iso.segment",
            Some("webm") => "video/webm",
            // audio
            Some("m3u8") => "application/vnd.apple.mpegurl",
            Some("m3u") => "audio/mpegurl",
            Some("mp3") => "audio/mpeg",
            Some("aac") => "audio/aac",
            Some("ogg") | Some("oga") => "audio/ogg",
            Some("wav") => "audio/wav",
            Some("opus") => "audio/opus",
            // text / markup
            Some("html") | Some("htm") => "text/html; charset=utf-8",
            Some("js") | Some("mjs") => "application/javascript",
            Some("css") => "text/css",
            Some("json") => "application/json",
            Some("xml") => "application/xml",
            Some("txt") | Some("log") => "text/plain; charset=utf-8",
            // images
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("svg") => "image/svg+xml",
            Some("webp") => "image/webp",
            Some("ico") => "image/x-icon",
            // fonts
            Some("woff") => "font/woff",
            Some("woff2") => "font/woff2",
            Some("ttf") => "font/ttf",
            Some("otf") => "font/otf",
            // other
            Some("wasm") => "application/wasm",
            Some("pdf") => "application/pdf",
            _ => "application/octet-stream",
        }
    }

    fn extract_range(request: &str) -> Option<(u64, Option<u64>)> {
        for line in request.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("range:") {
                let val = line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
                let spec = val.strip_prefix("bytes=")?;
                if spec.starts_with('-') {
                    return None; // suffix range not supported; serve whole file
                }
                let parts: Vec<&str> = spec.splitn(2, '-').collect();
                let start = parts[0].parse::<u64>().unwrap_or(0);
                let end = if parts.len() > 1 && !parts[1].is_empty() {
                    parts[1].parse::<u64>().ok()
                } else {
                    None
                };
                return Some((start, end));
            }
        }
        None
    }

    async fn handle_api(&mut self, path: &str) -> anyhow::Result<()> {
        let api = path.split_once('?').map(|(a, _)| a).unwrap_or(path);
        let api = api.trim_start_matches("/index/api/");

        match api {
            "getMediaList" => {
                let q = Self::parse_query(path);
                let fvhost = q.get("vhost").map(|s| s.as_str());
                let fapp = q.get("app").map(|s| s.as_str());
                let fstream = q.get("stream").map(|s| s.as_str());
                let sources = self.source_manager.list_filtered(fvhost, fapp, fstream);
                let mut list = Vec::new();
                for source in &sources {
                    let reader = source.subscriber_count().await;
                    let info = source.info.read().await;
                    let tracks: Vec<serde_json::Value> =
                        info.tracks.iter().map(Self::track_to_json).collect();
                    drop(info);
                    let alive = chrono::Utc::now()
                        .signed_duration_since(source.created_at)
                        .num_seconds()
                        .max(0);
                    list.push(serde_json::json!({
                        "app": source.app,
                        "stream": source.stream,
                        "vhost": source.vhost,
                        "url": source.url(),
                        "readerCount": reader,
                        "totalReaderCount": reader,
                        "createTime": source.created_at.timestamp_millis(),
                        "createStamp": source.created_at.timestamp(),
                        "aliveSecond": alive,
                        "tracks": tracks,
                    }));
                }
                self.send_json(&serde_json::json!({"code": 0, "result": list, "data": list}))
                    .await?;
            }
            "kick_session" => {
                let q = Self::parse_query(path);
                let id = q.get("id").map(|s| s.as_str()).unwrap_or("");
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__");
                let app = q.get("app").map(|s| s.as_str()).unwrap_or("live");
                let stream = q.get("stream").map(|s| s.as_str()).unwrap_or("");

                let mgr = match &self.session_manager {
                    Some(m) => m,
                    None => {
                        self.send_json(&serde_json::json!({
                            "code": -400,
                            "msg": "session manager not available"
                        }))
                        .await?;
                        return Ok(());
                    }
                };

                let mut count = 0usize;
                if !id.is_empty() {
                    if mgr.kick(id) {
                        count = 1;
                    }
                } else if !stream.is_empty() {
                    count = mgr.kick_by_stream(vhost, app, stream);
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing id or stream param"
                    }))
                    .await?;
                    return Ok(());
                }
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "result": { "count": count },
                    "msg": format!("kicked {} session(s)", count)
                }))
                .await?;
            }
            "getSnap" => {
                self.handle_get_snap(path).await?;
            }
            "getMediaInfo" => {
                let q = Self::parse_query(path);
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__");
                let app = q.get("app").map(|s| s.as_str()).unwrap_or("live");
                let stream = q.get("stream").map(|s| s.as_str()).unwrap_or("");
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                match self.source_manager.get(vhost, app, stream) {
                    Some(source) => {
                        let reader = source.subscriber_count().await;
                        let info = source.info.read().await;
                        let tracks: Vec<serde_json::Value> =
                            info.tracks.iter().map(Self::track_to_json).collect();
                        drop(info);
                        self.send_json(&serde_json::json!({
                            "code": 0,
                            "result": {
                                "app": source.app,
                                "stream": source.stream,
                                "vhost": source.vhost,
                                "url": source.url(),
                                "readerCount": reader,
                                "totalReaderCount": reader,
                                "createTime": source.created_at.timestamp_millis(),
                                "tracks": tracks,
                            }
                        }))
                        .await?;
                    }
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -404, "msg": "stream not found"}),
                        )
                        .await?;
                    }
                }
            }
            "getServerConfig" => {
                let cfg = zlmediakit_core::config::runtime_config_snapshot();
                let mut entries: Vec<serde_json::Value> = Vec::new();
                for (k, v) in cfg.iter() {
                    let value = if Self::is_secret_config_key(k) {
                        "******"
                    } else {
                        v
                    };
                    entries.push(serde_json::json!({ "key": k, "value": value }));
                }
                let secret_configured = !self.auth.secret().is_empty();
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "server": "zlmediakit-rs",
                    "version": env!("CARGO_PKG_VERSION"),
                    "api_version": "1.0",
                    "result": {
                        "data": entries,
                        "api": {
                            "apiDebug": cfg.get("api.apiDebug").cloned().unwrap_or_default(),
                            "secret": "******",
                            "secretConfigured": secret_configured,
                            "port": cfg.get("api.port").cloned().unwrap_or_default(),
                        },
                        "rtmp": { "port": cfg.get("rtmp.port").cloned().unwrap_or_default() },
                        "rtsp": { "port": cfg.get("rtsp.port").cloned().unwrap_or_default() },
                        "http": { "port": cfg.get("http.port").cloned().unwrap_or_default() },
                        "webrtc": {
                            "enabled": cfg.get("webrtc.enabled").cloned().unwrap_or_default(),
                            "port": cfg.get("webrtc.port").cloned().unwrap_or_default(),
                        },
                        "srt": {
                            "enabled": cfg.get("srt.enabled").cloned().unwrap_or_default(),
                            "port": cfg.get("srt.port").cloned().unwrap_or_default(),
                        },
                        "gb28181": {
                            "enabled": cfg.get("gb28181.enabled").cloned().unwrap_or_default(),
                            "sipPort": cfg.get("gb28181.sip_port").cloned().unwrap_or_default(),
                            "mediaPort": cfg.get("gb28181.media_port").cloned().unwrap_or_default(),
                        },
                        "record": {
                            "path": cfg.get("record.path").cloned().unwrap_or_default(),
                            "hls": cfg.get("record.hls").cloned().unwrap_or_default(),
                            "mp4": cfg.get("record.mp4").cloned().unwrap_or_default(),
                            "flv": cfg.get("record.flv").cloned().unwrap_or_default(),
                        },
                        "general": {
                            "mediaServerId": cfg.get("general.mediaServerId").cloned().unwrap_or_default(),
                            "flowThreshold": cfg.get("general.flowThreshold").cloned().unwrap_or_default(),
                        },
                    }
                }))
                .await?;
            }
            "setServerConfig" => {
                let q = Self::parse_query(path);
                let mut changed = 0usize;
                let mut restart_required = Vec::new();
                let mut rejected = Vec::new();
                for (k, v) in q {
                    if k == "secret" || k.is_empty() {
                        continue;
                    }
                    if !zlmediakit_core::config::has_runtime_config_key(&k) {
                        rejected.push(k);
                        continue;
                    }
                    if k.starts_with("hook.") {
                        match self.hook.set_config_value(&k, &v) {
                            Ok(hook_changed) => {
                                let snapshot_changed =
                                    zlmediakit_core::config::set_runtime_config(&k, &v);
                                if hook_changed || snapshot_changed {
                                    changed += 1;
                                }
                            }
                            Err(_) => rejected.push(k),
                        }
                        continue;
                    }
                    if zlmediakit_core::config::set_runtime_config(&k, &v) {
                        changed += 1;
                        if k == "api.secret" {
                            self.auth.set_secret(v);
                        } else {
                            restart_required.push(k);
                        }
                    }
                }
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "changed": changed,
                    "restartRequired": restart_required,
                    "rejected": rejected,
                }))
                .await?;
            }
            "version" => {
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "data": {
                        "branchName": "master",
                        "buildTime": "2024-01-01 00:00:00",
                        "commitHash": env!("CARGO_PKG_VERSION"),
                    }
                }))
                .await?;
            }
            "updateConfig" => {
                // Alias of setServerConfig: apply runtime-config updates and
                // report how many keys changed.
                let q = Self::parse_query(path);
                let mut changed = 0usize;
                let mut restart_required = Vec::new();
                let mut rejected = Vec::new();
                for (k, v) in q {
                    if k == "secret" || k.is_empty() {
                        continue;
                    }
                    if !zlmediakit_core::config::has_runtime_config_key(&k) {
                        rejected.push(k);
                        continue;
                    }
                    if k.starts_with("hook.") {
                        match self.hook.set_config_value(&k, &v) {
                            Ok(hook_changed) => {
                                let snapshot_changed =
                                    zlmediakit_core::config::set_runtime_config(&k, &v);
                                if hook_changed || snapshot_changed {
                                    changed += 1;
                                }
                            }
                            Err(_) => rejected.push(k),
                        }
                        continue;
                    }
                    if zlmediakit_core::config::set_runtime_config(&k, &v) {
                        changed += 1;
                        if k == "api.secret" {
                            self.auth.set_secret(v);
                        } else {
                            restart_required.push(k);
                        }
                    }
                }
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "changed": changed,
                    "restartRequired": restart_required,
                    "rejected": rejected,
                }))
                .await?;
            }
            "reloadCertificate" => {
                let report = zlmediakit_core::transport::reload_registered_tls();
                if report.failed.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": 0,
                        "reloaded": report.reloaded,
                    }))
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "one or more TLS certificates could not be reloaded; previous certificates remain active",
                        "reloaded": report.reloaded,
                        "failed": report.failed,
                    }))
                    .await?;
                }
            }
            "restartServer" => {
                // Respond first so the client sees success, then request a
                // graceful shutdown (a supervisor normally restarts the
                // process). The response is flushed before shutdown begins.
                self.send_json(&serde_json::json!({ "code": 0 })).await?;
                let _ = self.stream.flush().await;
                tokio::spawn(async {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    zlmediakit_core::signal::request_shutdown();
                });
            }
            "getApiList" => {
                const APIS: &[&str] = &[
                    "getApiList",
                    "updateConfig",
                    "restartServer",
                    "getThreadsLoad",
                    "getWorkThreadsLoad",
                    "getServerConfig",
                    "setServerConfig",
                    "reloadCertificate",
                    "getAllSession",
                    "kick_sessions",
                    "getMediaList",
                    "getMediaInfo",
                    "getMediaPlayerList",
                    "getStatistic",
                    "isMediaOnline",
                    "close_streams",
                    "closeStream",
                    "kick_session",
                    "getSnap",
                    "getMp4RecordFile",
                    "getRecordStatus",
                    "startRecord",
                    "stopRecord",
                    "isRecording",
                    "addStreamPusher",
                    "delStreamPusher",
                    "getStreamPusherList",
                    "addStreamProxy",
                    "delStreamProxy",
                    "getStreamProxyList",
                    "addFFmpegSource",
                    "delFFmpegSource",
                    "getFFmpegSourceList",
                    "openRtpServer",
                    "connectRtpServer",
                    "closeRtpServer",
                    "listRtpServer",
                    "getRtpInfo",
                    "startSendRtp",
                    "startSendRtpPassive",
                    "listRtpSender",
                    "stopSendRtp",
                    "startRtp",
                    "stopRtp",
                    "startTalk",
                    "stopTalk",
                    "getTalkList",
                    "queryCatalog",
                    "queryDeviceInfo",
                    "getDeviceList",
                    "getDeviceInfo",
                    "getSipInfo",
                    "stopSip",
                    "addTranscode",
                    "delTranscode",
                    "getTranscode",
                    "getTranscodeList",
                ];
                let data: Vec<String> = APIS.iter().map(|a| format!("/index/api/{}", a)).collect();
                self.send_json(&serde_json::json!({ "code": 0, "data": data }))
                    .await?;
            }
            "getThreadsLoad" | "getWorkThreadsLoad" => {
                let threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1);
                let data: Vec<serde_json::Value> = (0..threads)
                    .map(|i| {
                        serde_json::json!({
                            "delay": 0,
                            "load": 0,
                            "thread_id": i,
                        })
                    })
                    .collect();
                self.send_json(&serde_json::json!({ "code": 0, "data": data }))
                    .await?;
            }
            "getAllSession" => {
                let mgr = match &self.session_manager {
                    Some(m) => m,
                    None => {
                        self.send_json(&serde_json::json!({
                            "code": -400,
                            "msg": "session manager not available"
                        }))
                        .await?;
                        return Ok(());
                    }
                };
                let q = Self::parse_query(path);
                let local_port: Option<u16> = q.get("local_port").and_then(|v| v.parse().ok());
                let peer_ip = q.get("peer_ip").map(|s| s.as_str());
                let data: Vec<serde_json::Value> = mgr
                    .list()
                    .iter()
                    .filter(|info| {
                        let ip_ok = peer_ip.is_none_or(|ip| {
                            info.peer_addr.split(':').next().unwrap_or("") == ip
                        });
                        let port_ok = local_port.is_none_or(|p| {
                            info.local_addr
                                .split(':')
                                .next_back()
                                .and_then(|s| s.parse::<u16>().ok())
                                == Some(p)
                        });
                        ip_ok && port_ok
                    })
                    .map(|info| {
                        let (peer_ip, peer_port) = split_addr(&info.peer_addr);
                        let (local_ip, lport) = split_addr(&info.local_addr);
                        serde_json::json!({
                            "id": info.id,
                            "peer_ip": peer_ip,
                            "peer_port": peer_port,
                            "local_ip": local_ip,
                            "local_port": lport,
                            "typeid": info.protocol,
                            "created_at": info.created_at.timestamp(),
                            "stream": info.stream.as_ref().map(|(v, a, s)| format!("{}/{}/{}", v, a, s)),
                        })
                    })
                    .collect();
                self.send_json(&serde_json::json!({ "code": 0, "data": data }))
                    .await?;
            }
            "kick_sessions" => {
                let mgr = match &self.session_manager {
                    Some(m) => m,
                    None => {
                        self.send_json(&serde_json::json!({
                            "code": -400,
                            "msg": "session manager not available"
                        }))
                        .await?;
                        return Ok(());
                    }
                };
                let q = Self::parse_query(path);
                let peer_ip = q.get("peer_ip").map(|s| s.as_str());
                let local_port: Option<u16> = q.get("local_port").and_then(|v| v.parse().ok());
                let protocol = q.get("typeid").map(|s| s.as_str());
                let count = mgr.kick_by_filter(peer_ip, local_port, protocol);
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "count_hit": count,
                    "msg": format!("kicked {} session(s)", count)
                }))
                .await?;
            }
            "isMediaOnline" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing stream param"
                    }))
                    .await?;
                    return Ok(());
                }
                let online = self.source_manager.get(&vhost, &app, &stream).is_some();
                self.send_json(&serde_json::json!({ "code": 0, "online": online }))
                    .await?;
            }
            "close_streams" => {
                let q = Self::parse_query(path);
                let vhost = q.get("vhost").map(|s| s.as_str());
                let app = q.get("app").map(|s| s.as_str());
                let stream = q.get("stream").map(|s| s.as_str());
                let mut count = 0usize;
                let ids: Vec<(String, String, String)> = self
                    .source_manager
                    .list_filtered(vhost, app, stream)
                    .iter()
                    .map(|s| (s.vhost.clone(), s.app.clone(), s.stream.clone()))
                    .collect();
                for (v, a, s) in ids {
                    if self.source_manager.close_stream(&v, &a, &s) {
                        count += 1;
                    }
                }
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "count_closed": count,
                    "msg": format!("closed {} stream(s)", count)
                }))
                .await?;
            }
            "getMediaPlayerList" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing stream param"
                    }))
                    .await?;
                    return Ok(());
                }
                match self.source_manager.get(&vhost, &app, &stream) {
                    Some(source) => {
                        let players: Vec<String> = source.subscriber_ids().await;
                        let mut data = Vec::new();
                        if let Some(mgr) = &self.session_manager {
                            for id in &players {
                                if let Some(info) = mgr.get(id) {
                                    let (peer_ip, peer_port) = split_addr(&info.peer_addr);
                                    let (_, local_port) = split_addr(&info.local_addr);
                                    data.push(serde_json::json!({
                                        "playerId": info.id,
                                        "peer_ip": peer_ip,
                                        "peer_port": peer_port,
                                        "local_port": local_port,
                                        "protocol": info.protocol,
                                        "aliveSecond": chrono::Utc::now()
                                            .signed_duration_since(info.created_at)
                                            .num_seconds()
                                            .max(0),
                                    }));
                                } else {
                                    data.push(serde_json::json!({
                                        "playerId": id,
                                    }));
                                }
                            }
                        } else {
                            for id in &players {
                                data.push(serde_json::json!({ "playerId": id }));
                            }
                        }
                        self.send_json(&serde_json::json!({
                            "code": 0,
                            "data": data
                        }))
                        .await?;
                    }
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -404, "msg": "stream not found"}),
                        )
                        .await?;
                    }
                }
            }
            "getRecordStatus" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let state = self
                    .recorder
                    .is_recording(&vhost, &app, &stream)
                    .unwrap_or((false, false, false));
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "status": state.0 || state.1 || state.2,
                    "recording": {
                        "hls": state.0,
                        "flv": state.1,
                        "mp4": state.2,
                    }
                }))
                .await?;
            }
            "getMp4RecordFile" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                let q = Self::parse_query(path);
                let period = q.get("period").cloned().unwrap_or_default();
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let base = self.record_root.join(&app).join(&stream);
                let files = list_record_files(&base, &period, &vhost, &app, &stream);
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "data": files
                }))
                .await?;
            }
            "getStatistic" => {
                let sources = self.source_manager.list();
                let mut data = Vec::new();
                for source in &sources {
                    let reader = source.subscriber_count().await;
                    let info = source.info.read().await;
                    let track_count = info.tracks.len();
                    drop(info);
                    let alive = chrono::Utc::now()
                        .signed_duration_since(source.created_at)
                        .num_seconds()
                        .max(0);
                    data.push(serde_json::json!({
                        "app": source.app,
                        "stream": source.stream,
                        "vhost": source.vhost,
                        "url": source.url(),
                        "readerCount": reader,
                        "totalReaderCount": reader,
                        "createTime": source.created_at.timestamp_millis(),
                        "aliveSecond": alive,
                        "trackCount": track_count,
                    }));
                }
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "result": {
                        "totalStreams": sources.len(),
                        "data": data,
                    }
                }))
                .await?;
            }
            "addTranscode" => {
                let mgr = match &self.transcode {
                    Some(m) => m.clone(),
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -1, "msg": "transcode disabled"}),
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let q = Self::parse_query(path);
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__")
                    .to_string();
                let app = q
                    .get("app")
                    .map(|s| s.as_str())
                    .unwrap_or("live")
                    .to_string();
                let stream = match q.get("stream").map(|s| s.as_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        self.send_json(&serde_json::json!({"code": -400, "msg": "missing stream"}))
                            .await?;
                        return Ok(());
                    }
                };
                let input_codec = q.get("acodec").map(|s| s.as_str()).unwrap_or("");
                let codec = q.get("code").map(|s| s.as_str()).unwrap_or("H264");
                // ZLMediaKit uses numeric-ish / named codes; normalize.
                let output_codec = match codec.to_uppercase().as_str() {
                    "H264" | "H_264" | "264" | "AVC" | "7" => "h264",
                    "H265" | "H_265" | "265" | "HEVC" | "16777216" => "h265",
                    _ => "h264",
                };
                let scale = q.get("scale").and_then(|s| {
                    let mut it = s.split('x');
                    let w = it.next()?.parse::<u32>().ok()?;
                    let h = it.next()?.parse::<u32>().ok()?;
                    Some((w, h))
                });
                let bitrate = q.get("bitrate").map(|s| s.as_str());
                let name = q.get("name").map(|s| s.as_str());
                let dst_app = q.get("dst_app").map(|s| s.as_str());
                let dst_stream = q.get("dst_stream").map(|s| s.as_str());
                let config = TranscodeConfig::from_api_params(
                    input_codec,
                    output_codec,
                    scale,
                    bitrate,
                    name,
                    dst_app,
                    dst_stream,
                    None,
                );
                match mgr.add_transcode(&vhost, &app, &stream, config).await {
                    Ok(info) => {
                        self.send_json(&serde_json::json!({
                            "code": 0,
                            "result": {
                                "key": info.key,
                                "dst_app": info.dst_app,
                                "dst_stream": info.dst_stream,
                                "url": format!("{}/{}/{}", vhost, info.dst_app, info.dst_stream),
                            }
                        }))
                        .await?;
                    }
                    Err(e) => {
                        self.send_json(&serde_json::json!({"code": -1, "msg": e.to_string()}))
                            .await?;
                    }
                }
            }
            "delTranscode" => {
                let mgr = match &self.transcode {
                    Some(m) => m.clone(),
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -1, "msg": "transcode disabled"}),
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let q = Self::parse_query(path);
                let key = q.get("key").map(|s| s.as_str()).unwrap_or("").to_string();
                let ok = if key.is_empty() {
                    false
                } else {
                    mgr.del_transcode(&key)
                };
                self.send_json(&serde_json::json!({"code": 0, "result": ok}))
                    .await?;
            }
            "getTranscode" => {
                let mgr = match &self.transcode {
                    Some(m) => m.clone(),
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -1, "msg": "transcode disabled"}),
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let q = Self::parse_query(path);
                let key = q.get("key").map(|s| s.as_str()).unwrap_or("").to_string();
                match mgr.get_transcode(&key).await {
                    Some(info) => {
                        self.send_json(&serde_json::to_value(&info).unwrap())
                            .await?
                    }
                    None => {
                        self.send_json(&serde_json::json!({"code": -404, "msg": "not found"}))
                            .await?
                    }
                }
            }
            "getTranscodeList" => {
                let mgr = match &self.transcode {
                    Some(m) => m.clone(),
                    None => {
                        self.send_json(
                            &serde_json::json!({"code": -1, "msg": "transcode disabled"}),
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let list = mgr.list_transcode().await;
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "result": list.iter().map(|i| serde_json::to_value(i).unwrap()).collect::<Vec<_>>()
                }))
                .await?;
            }
            "closeStream" => {
                let q = Self::parse_query(path);
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__");
                let app = q.get("app").map(|s| s.as_str()).unwrap_or("live");
                let stream = q.get("stream").map(|s| s.as_str()).unwrap_or("");
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let ok = self.source_manager.close_stream(vhost, app, stream);
                if ok {
                    self.send_json(&serde_json::json!({"code": 0, "result": "stream closed"}))
                        .await?;
                } else {
                    self.send_json(&serde_json::json!({"code": -404, "msg": "stream not found"}))
                        .await?;
                }
            }
            "startRecord" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                if self.source_manager.get(&vhost, &app, &stream).is_none() {
                    self.send_json(&serde_json::json!({"code": -404, "msg": "stream not found"}))
                        .await?;
                    return Ok(());
                }
                let (hls, flv, mp4) = Self::api_record_types(path);
                self.recorder.start(&vhost, &app, &stream, hls, flv, mp4);
                self.send_json(&serde_json::json!({"code": 0, "result": "record started"}))
                    .await?;
            }
            "stopRecord" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                self.recorder.stop(&vhost, &app, &stream);
                self.send_json(&serde_json::json!({"code": 0, "result": "record stopped"}))
                    .await?;
            }
            "isRecording" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let state = self
                    .recorder
                    .is_recording(&vhost, &app, &stream)
                    .unwrap_or((false, false, false));
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "result": {
                        "recording": state.0 || state.1 || state.2,
                        "hls": state.0,
                        "flv": state.1,
                        "mp4": state.2,
                    }
                }))
                .await?;
            }
            "addStreamPusher" => {
                let q = Self::parse_query(path);
                let dst_url = q
                    .get("dst_url")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__")
                    .to_string();
                let app = q
                    .get("app")
                    .map(|s| s.as_str())
                    .unwrap_or("live")
                    .to_string();
                let stream = q
                    .get("stream")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                if dst_url.is_empty() || stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing dst_url or stream param"
                    }))
                    .await?;
                    return Ok(());
                }
                let ok = self.pusher.add(&dst_url, &vhost, &app, &stream);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "stream pusher started"}),
                    )
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "pusher already exists"
                    }))
                    .await?;
                }
            }
            "delStreamPusher" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let ok = self.pusher.remove(&vhost, &app, &stream);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "stream pusher stopped"}),
                    )
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({"code": -404, "msg": "pusher not found"}))
                        .await?;
                }
            }
            "getStreamPusherList" => {
                let list = self.pusher.list();
                let data: Vec<serde_json::Value> = list
                    .iter()
                    .map(|(dst_url, vhost, app, stream)| {
                        serde_json::json!({
                            "dst_url": dst_url,
                            "vhost": vhost,
                            "app": app,
                            "stream": stream,
                        })
                    })
                    .collect();
                self.send_json(&serde_json::json!({"code": 0, "result": data}))
                    .await?;
            }
            "addFFmpegSource" => {
                let q = Self::parse_query(path);
                let src_url = q
                    .get("src_url")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let dst_url = q
                    .get("dst_url")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__")
                    .to_string();
                let app = q
                    .get("app")
                    .map(|s| s.as_str())
                    .unwrap_or("live")
                    .to_string();
                let stream = q
                    .get("stream")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let timeout_ms: u32 = q
                    .get("timeout_ms")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(10000);
                if src_url.is_empty() || dst_url.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing src_url or dst_url param"
                    }))
                    .await?;
                    return Ok(());
                }
                let stream = if stream.is_empty() {
                    dst_url.rsplit('/').next().unwrap_or("stream").to_string()
                } else {
                    stream
                };
                let ok = self
                    .ffmpeg
                    .add(&src_url, &dst_url, &vhost, &app, &stream, timeout_ms);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "ffmpeg source started"}),
                    )
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "ffmpeg source already exists"
                    }))
                    .await?;
                }
            }
            "delFFmpegSource" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let ok = self.ffmpeg.remove(&vhost, &app, &stream);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "ffmpeg source stopped"}),
                    )
                    .await?;
                } else {
                    self.send_json(
                        &serde_json::json!({"code": -404, "msg": "ffmpeg source not found"}),
                    )
                    .await?;
                }
            }
            "getFFmpegSourceList" => {
                let list = self.ffmpeg.list();
                let data: Vec<serde_json::Value> = list
                    .iter()
                    .map(|(src_url, dst_url, vhost, app, stream)| {
                        serde_json::json!({
                            "src_url": src_url,
                            "dst_url": dst_url,
                            "vhost": vhost,
                            "app": app,
                            "stream": stream,
                        })
                    })
                    .collect();
                self.send_json(&serde_json::json!({"code": 0, "result": data}))
                    .await?;
            }
            "addStreamProxy" => {
                let q = Self::parse_query(path);
                let url = q.get("url").map(|s| s.as_str()).unwrap_or("").to_string();
                let vhost = q
                    .get("vhost")
                    .map(|s| s.as_str())
                    .unwrap_or("__defaultVhost__")
                    .to_string();
                let app = q
                    .get("app")
                    .map(|s| s.as_str())
                    .unwrap_or("live")
                    .to_string();
                let stream = q
                    .get("stream")
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() || stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -400,
                        "msg": "missing url or stream param"
                    }))
                    .await?;
                    return Ok(());
                }
                let ok = self.proxy.add(&url, &vhost, &app, &stream);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "stream proxy started"}),
                    )
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "proxy already exists"
                    }))
                    .await?;
                }
            }
            "delStreamProxy" => {
                let (vhost, app, stream) = Self::api_stream_params(path);
                if stream.is_empty() {
                    self.send_json(
                        &serde_json::json!({"code": -400, "msg": "missing stream param"}),
                    )
                    .await?;
                    return Ok(());
                }
                let ok = self.proxy.remove(&vhost, &app, &stream);
                if ok {
                    self.send_json(
                        &serde_json::json!({"code": 0, "result": "stream proxy stopped"}),
                    )
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({"code": -404, "msg": "proxy not found"}))
                        .await?;
                }
            }
            "getStreamProxyList" => {
                let list = self.proxy.list();
                let data: Vec<serde_json::Value> = list
                    .iter()
                    .map(|(url, vhost, app, stream)| {
                        serde_json::json!({
                            "url": url,
                            "vhost": vhost,
                            "app": app,
                            "stream": stream,
                        })
                    })
                    .collect();
                self.send_json(&serde_json::json!({"code": 0, "result": data}))
                    .await?;
            }
            "openRtpServer" => {
                if let Some(rtp) = &self.rtp {
                    let q = Self::parse_query(path);
                    let port: u16 = q.get("port").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let ssrc: Option<u32> = q.get("ssrc").and_then(|v| v.parse().ok());
                    let app = q.get("app").cloned().unwrap_or_else(|| "rtp".to_string());
                    let stream = q
                        .get("stream_id")
                        .cloned()
                        .or_else(|| q.get("stream").cloned())
                        .unwrap_or_else(|| format!("rtp_{port}"));
                    let payload = q
                        .get("type")
                        .map(|s| RtpPayloadType::parse(s))
                        .unwrap_or(RtpPayloadType::Auto);
                    let tcp_mode = q
                        .get("tcp_mode")
                        .and_then(|value| value.parse::<u8>().ok())
                        .unwrap_or(0);
                    let vhost = q
                        .get("vhost")
                        .map(|s| s.as_str())
                        .unwrap_or("__defaultVhost__")
                        .to_string();
                    let tcp_mode = match RtpTcpMode::try_from(tcp_mode) {
                        Ok(mode) => mode,
                        Err(error) => {
                            let _ = self
                                .send_json(&serde_json::json!({
                                    "code": -1,
                                    "msg": error.to_string()
                                }))
                                .await;
                            return Ok(());
                        }
                    };
                    match rtp
                        .open_with_mode(port, &vhost, &app, &stream, payload, ssrc, tcp_mode)
                        .await
                    {
                        Ok(p) => {
                            let _ = self
                                .send_json(&serde_json::json!({
                                    "code": 0,
                                    "port": p,
                                    "result": p
                                }))
                                .await;
                        }
                        Err(e) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": -1, "msg": e.to_string()}))
                                .await;
                        }
                    }
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "rtp server disabled"}))
                        .await;
                }
            }
            "connectRtpServer" => {
                if let Some(rtp) = &self.rtp {
                    let q = Self::parse_query(path);
                    let stream = q
                        .get("stream_id")
                        .cloned()
                        .or_else(|| q.get("stream").cloned())
                        .unwrap_or_default();
                    let app = q.get("app").map(String::as_str);
                    let dst_host = q.get("dst_url").cloned().unwrap_or_default();
                    let dst_port = q
                        .get("dst_port")
                        .and_then(|value| value.parse::<u16>().ok())
                        .unwrap_or(0);
                    if stream.is_empty() || dst_host.is_empty() || dst_port == 0 {
                        self.send_json(&serde_json::json!({
                            "code": -300,
                            "msg": "stream_id, dst_url and dst_port are required"
                        }))
                        .await?;
                        return Ok(());
                    }
                    match rtp.connect_stream(app, &stream, &dst_host, dst_port).await {
                        Ok(local_port) => {
                            self.send_json(&serde_json::json!({
                                "code": 0,
                                "local_port": local_port,
                                "result": true
                            }))
                            .await?;
                        }
                        Err(error) => {
                            self.send_json(&serde_json::json!({
                                "code": -1,
                                "msg": error.to_string()
                            }))
                            .await?;
                        }
                    }
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "rtp server disabled"
                    }))
                    .await?;
                }
            }
            "closeRtpServer" => {
                if let Some(rtp) = &self.rtp {
                    let q = Self::parse_query(path);
                    let port: u16 = q.get("port").and_then(|v| v.parse().ok()).unwrap_or(0);
                    rtp.close(port);
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": "ok"}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "rtp server disabled"}))
                        .await;
                }
            }
            "listRtpServer" => {
                if let Some(rtp) = &self.rtp {
                    let list = rtp.list();
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": list}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "rtp server disabled"}))
                        .await;
                }
            }
            "getRtpInfo" => {
                if let Some(rtp) = &self.rtp {
                    let q = Self::parse_query(path);
                    let port: u16 = q.get("port").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let info = rtp.get_info(port);
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": info}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "rtp server disabled"}))
                        .await;
                }
            }
            "startSendRtp" | "startSendRtpPassive" => {
                let q = Self::parse_query(path);
                let passive = api == "startSendRtpPassive";
                let vhost = q
                    .get("vhost")
                    .cloned()
                    .unwrap_or_else(|| "__defaultVhost__".to_string());
                let app = q.get("app").cloned().unwrap_or_else(|| "live".to_string());
                let stream = q.get("stream").cloned().unwrap_or_default();
                let dst_host = q.get("dst_url").cloned().unwrap_or_default();
                let dst_port = q
                    .get("dst_port")
                    .and_then(|value| value.parse::<u16>().ok());
                let ssrc = q.get("ssrc").and_then(|value| {
                    value
                        .strip_prefix("0x")
                        .or_else(|| value.strip_prefix("0X"))
                        .map_or_else(
                            || value.parse::<u32>().ok(),
                            |hex| u32::from_str_radix(hex, 16).ok(),
                        )
                });
                if stream.is_empty()
                    || ssrc.is_none()
                    || (!passive && (dst_host.is_empty() || dst_port.is_none()))
                {
                    self.send_json(&serde_json::json!({
                        "code": -300,
                        "msg": if passive {
                            "stream and ssrc are required"
                        } else {
                            "stream, ssrc, dst_url and dst_port are required"
                        }
                    }))
                    .await?;
                    return Ok(());
                }
                let data_type_value = if let Some(value) = q.get("use_ps") {
                    if value == "1" || value.eq_ignore_ascii_case("true") {
                        1
                    } else {
                        0
                    }
                } else {
                    q.get("type")
                        .and_then(|value| value.parse::<u8>().ok())
                        .unwrap_or(1)
                };
                let data_type = match RtpDataType::try_from(data_type_value) {
                    Ok(value) => value,
                    Err(error) => {
                        self.send_json(&serde_json::json!({
                            "code": -300,
                            "msg": error.to_string()
                        }))
                        .await?;
                        return Ok(());
                    }
                };
                let request = RtpSendRequest {
                    vhost,
                    app,
                    stream,
                    ssrc: ssrc.expect("validated above"),
                    dst_host,
                    dst_port: dst_port.unwrap_or(0),
                    src_port: q
                        .get("src_port")
                        .and_then(|value| value.parse::<u16>().ok())
                        .unwrap_or(0),
                    payload_type: q
                        .get("pt")
                        .and_then(|value| value.parse::<u8>().ok())
                        .unwrap_or(96),
                    is_udp: if passive {
                        false
                    } else {
                        q.get("is_udp")
                            .is_none_or(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                    },
                    data_type,
                    only_audio: q
                        .get("only_audio")
                        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
                    passive,
                    ssrc_multi_send: q
                        .get("ssrc_multi_send")
                        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true")),
                };
                match self.rtp_sender.start(request).await {
                    Ok(local_port) => {
                        self.send_json(&serde_json::json!({
                            "code": 0,
                            "local_port": local_port
                        }))
                        .await?;
                    }
                    Err(error) => {
                        self.send_json(&serde_json::json!({
                            "code": -1,
                            "msg": error.to_string()
                        }))
                        .await?;
                    }
                }
            }
            "listRtpSender" => {
                let q = Self::parse_query(path);
                let vhost = q
                    .get("vhost")
                    .map(String::as_str)
                    .unwrap_or("__defaultVhost__");
                let app = q.get("app").map(String::as_str).unwrap_or("live");
                let stream = q.get("stream").map(String::as_str).unwrap_or("");
                if stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -300,
                        "msg": "stream is required"
                    }))
                    .await?;
                    return Ok(());
                }
                let senders: Vec<_> = self
                    .rtp_sender
                    .list()
                    .into_iter()
                    .filter(|sender| {
                        sender.vhost == vhost && sender.app == app && sender.stream == stream
                    })
                    .collect();
                let data: Vec<String> = senders
                    .iter()
                    .map(|sender| sender.ssrc.to_string())
                    .collect();
                let details: Vec<_> = senders
                    .iter()
                    .map(|sender| {
                        serde_json::json!({
                            "ssrc": sender.ssrc.to_string(),
                            "destination": sender.destination.map(|value| value.to_string()),
                            "local_port": sender.local_port,
                            "packets": sender.packets,
                            "totalBytes": sender.bytes,
                        })
                    })
                    .collect();
                let total_bytes: u64 = senders.iter().map(|sender| sender.bytes).sum();
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "data": data,
                    "details": details,
                    "bytesSpeed": 0,
                    "totalBytes": total_bytes,
                }))
                .await?;
            }
            "stopSendRtp" => {
                let q = Self::parse_query(path);
                let vhost = q
                    .get("vhost")
                    .map(String::as_str)
                    .unwrap_or("__defaultVhost__");
                let app = q.get("app").map(String::as_str).unwrap_or("live");
                let stream = q.get("stream").map(String::as_str).unwrap_or("");
                if stream.is_empty() {
                    self.send_json(&serde_json::json!({
                        "code": -300,
                        "msg": "stream is required"
                    }))
                    .await?;
                    return Ok(());
                }
                let ssrc = q.get("ssrc").and_then(|value| value.parse::<u32>().ok());
                let stopped = self.rtp_sender.stop(vhost, app, stream, ssrc);
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "stopped": stopped
                }))
                .await?;
            }
            "startRtp" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let device = q.get("device_id").cloned().unwrap_or_default();
                    let channel = q
                        .get("channel_id")
                        .or_else(|| q.get("stream"))
                        .cloned()
                        .unwrap_or_default();
                    match sip.invite(&device, &channel).await {
                        Ok(port) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": 0, "result": port}))
                                .await;
                        }
                        Err(e) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": -1, "msg": e.to_string()}))
                                .await;
                        }
                    }
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "stopRtp" => {
                if let Some(rtp) = &self.rtp {
                    let q = Self::parse_query(path);
                    if let Some(stream) = q.get("stream").or_else(|| q.get("channel_id")) {
                        let app = q
                            .get("app")
                            .cloned()
                            .unwrap_or_else(|| "gb28181".to_string());
                        if let Some(info) = rtp.find_by_stream(&app, stream) {
                            rtp.close(info.port);
                        }
                        if let Some(sip) = &self.sip {
                            let _ = sip.bye_stream(stream);
                        }
                    }
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": "ok"}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "rtp server disabled"}))
                        .await;
                }
            }
            "startTalk" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let device_id = q.get("device_id").cloned().unwrap_or_default();
                    let channel_id = q.get("channel_id").cloned().unwrap_or_default();
                    let vhost = q
                        .get("vhost")
                        .map(String::as_str)
                        .unwrap_or("__defaultVhost__");
                    let app = q.get("app").map(String::as_str).unwrap_or("live");
                    let stream = q.get("stream").map(String::as_str).unwrap_or("");
                    if device_id.is_empty() || channel_id.is_empty() || stream.is_empty() {
                        self.send_json(&serde_json::json!({
                            "code": -300,
                            "msg": "device_id, channel_id and stream are required"
                        }))
                        .await?;
                        return Ok(());
                    }
                    match sip
                        .start_talk(&device_id, &channel_id, vhost, app, stream)
                        .await
                    {
                        Ok(local_port) => {
                            self.send_json(&serde_json::json!({
                                "code": 0,
                                "local_port": local_port
                            }))
                            .await?;
                        }
                        Err(error) => {
                            self.send_json(&serde_json::json!({
                                "code": -1,
                                "msg": error.to_string()
                            }))
                            .await?;
                        }
                    }
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "gb28181 disabled"
                    }))
                    .await?;
                }
            }
            "stopTalk" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let channel_id = q.get("channel_id").cloned().unwrap_or_default();
                    let stopped = !channel_id.is_empty() && sip.stop_talk(&channel_id);
                    self.send_json(&serde_json::json!({
                        "code": 0,
                        "stopped": stopped
                    }))
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "gb28181 disabled"
                    }))
                    .await?;
                }
            }
            "getTalkList" => {
                if let Some(sip) = &self.sip {
                    self.send_json(&serde_json::json!({
                        "code": 0,
                        "data": sip.list_talks()
                    }))
                    .await?;
                } else {
                    self.send_json(&serde_json::json!({
                        "code": -1,
                        "msg": "gb28181 disabled"
                    }))
                    .await?;
                }
            }
            "queryCatalog" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let device_id = q.get("device_id").cloned().unwrap_or_default();
                    match sip.query_catalog(&device_id).await {
                        Ok(channels) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": 0, "result": channels}))
                                .await;
                        }
                        Err(e) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": -1, "msg": e.to_string()}))
                                .await;
                        }
                    }
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "queryDeviceInfo" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let device_id = q.get("device_id").cloned().unwrap_or_default();
                    match sip.query_device_info(&device_id).await {
                        Ok(info) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": 0, "result": info}))
                                .await;
                        }
                        Err(e) => {
                            let _ = self
                                .send_json(&serde_json::json!({"code": -1, "msg": e.to_string()}))
                                .await;
                        }
                    }
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "getDeviceList" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let list = match q.get("device_id") {
                        Some(id) => {
                            let mut out = Vec::new();
                            if let Some(dev) = sip.get_device(id) {
                                out.push(dev);
                            }
                            out
                        }
                        None => sip.list_devices(),
                    };
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "data": list}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "getDeviceInfo" => {
                if let Some(sip) = &self.sip {
                    let q = Self::parse_query(path);
                    let device_id = q.get("device_id").cloned().unwrap_or_default();
                    let _ = self
                        .send_json(
                            &serde_json::json!({"code": 0, "result": sip.get_device(&device_id)}),
                        )
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "getSipInfo" => {
                if let Some(sip) = &self.sip {
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": sip.info()}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            "stopSip" => {
                if let Some(sip) = &self.sip {
                    sip.stop();
                    let _ = self
                        .send_json(&serde_json::json!({"code": 0, "result": "sip stopped"}))
                        .await;
                } else {
                    let _ = self
                        .send_json(&serde_json::json!({"code": -1, "msg": "gb28181 disabled"}))
                        .await;
                }
            }
            _ => {
                self.send_404().await?;
            }
        }

        Ok(())
    }

    /// Extracts `(vhost, app, stream)` from the request query, defaulting
    /// `vhost` to `__defaultVhost__` and `app` to `live`.
    fn api_stream_params(path: &str) -> (String, String, String) {
        let q = Self::parse_query(path);
        let vhost = q
            .get("vhost")
            .map(|s| s.as_str())
            .unwrap_or("__defaultVhost__")
            .to_string();
        let app = q
            .get("app")
            .map(|s| s.as_str())
            .unwrap_or("live")
            .to_string();
        let stream = q
            .get("stream")
            .map(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        (vhost, app, stream)
    }

    /// Decides which container formats to record. Accepts ZLMediaKit's `type`
    /// param (`hls`/`flv`/`mp4`/`all`) and the boolean flags `hls=1`/`flv=1`/
    /// `mp4=1`. Defaults to HLS when nothing is requested.
    fn api_record_types(path: &str) -> (bool, bool, bool) {
        let q = Self::parse_query(path);
        let t = q
            .get("type")
            .map(|s| s.as_str())
            .unwrap_or("")
            .to_lowercase();
        let flag = |k: &str| q.get(k).map(|v| v == "1").unwrap_or(false);
        let hls = t == "hls" || t == "all" || flag("hls");
        let flv = t == "flv" || t == "all" || flag("flv");
        let mp4 = t == "mp4" || t == "all" || flag("mp4");
        if hls || flv || mp4 {
            (hls, flv, mp4)
        } else {
            (true, false, false)
        }
    }

    async fn send_json(&mut self, body: &serde_json::Value) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(body)?;
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    /// Authorizes management API requests with the configured shared secret.
    /// The dedicated request header keeps the secret out of normal URLs while
    /// the query parameter remains compatible with the upstream ZLMediaKit API.
    fn is_api_authorized(&self, path: &str, request: &str) -> bool {
        let expected = self.auth.secret();
        if expected.is_empty() {
            return true;
        }

        let header_secret = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("x-api-secret") {
                Some(value.trim())
            } else if name.trim().eq_ignore_ascii_case("authorization") {
                value.trim().strip_prefix("Bearer ")
            } else {
                None
            }
        });
        let query = Self::parse_query(path);
        let supplied = header_secret.or_else(|| query.get("secret").map(String::as_str));

        supplied.is_some_and(|candidate| Self::constant_time_eq(&expected, candidate))
    }

    fn constant_time_eq(expected: &str, candidate: &str) -> bool {
        if expected.len() != candidate.len() {
            return false;
        }
        expected
            .as_bytes()
            .iter()
            .zip(candidate.as_bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    fn is_secret_config_key(key: &str) -> bool {
        key.eq_ignore_ascii_case("secret") || key.to_ascii_lowercase().ends_with(".secret")
    }

    async fn send_api_unauthorized(&mut self) -> anyhow::Result<()> {
        let body = serde_json::to_string(&serde_json::json!({
            "code": -401,
            "msg": "invalid or missing API secret"
        }))?;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\n\
             Content-Type: application/json\r\n\
             Cache-Control: no-store\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    fn track_to_json(track: &zlmediakit_core::media_frame::TrackInfo) -> serde_json::Value {
        match track {
            zlmediakit_core::media_frame::TrackInfo::Video(v) => serde_json::json!({
                "codec_type": "video",
                "codec_id": format!("{:?}", v.codec),
                "width": v.width,
                "height": v.height,
                "fps": v.fps,
            }),
            zlmediakit_core::media_frame::TrackInfo::Audio(a) => serde_json::json!({
                "codec_type": "audio",
                "codec_id": format!("{:?}", a.codec),
                "sample_rate": a.sample_rate,
                "channels": a.channels,
                "bits_per_sample": a.bits_per_sample,
            }),
        }
    }

    /// Splits a request path into `(app, resource, sign)`, extracting the
    /// `sign` query parameter (used for token-based playback auth). `resource`
    /// is everything after the app segment, with the query string removed so
    /// callers can strip their own suffix (`.flv`, `.m3u8`, ...).
    fn parse_path_sign(path: &str) -> (String, String, String) {
        let path = path.trim_start_matches('/');
        let (path_only, sign) = match path.split_once('?') {
            Some((p, q)) => {
                let sign = q
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(k, _)| *k == "sign")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                (p, sign)
            }
            None => (path, String::new()),
        };
        let parts: Vec<&str> = path_only.splitn(2, '/').collect();
        let app = parts.first().map_or("live", |v| v).to_string();
        let resource = parts.get(1).map_or("", |v| v).to_string();
        (app, resource, sign)
    }

    /// Returns `true` for request paths that map to a live stream playback
    /// route (HTTP-FLV / HTTP-TS / HTTP-fMP4 / HLS / CMAF / DASH). Static
    /// files, the API and the web root are not considered playback routes.
    fn is_play_route(route: &str) -> bool {
        route.ends_with(".flv")
            || route.ends_with(".live.ts")
            || route.ends_with(".live.mp4")
            || route.ends_with(".m3u8")
            || route.ends_with(".cmav.m3u8")
            || route.ends_with(".mpd")
            || route.ends_with(".m4s")
            || route.ends_with(".ts")
    }

    fn parse_query(path: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if let Some(q) = path.split_once('?').map(|(_, q)| q) {
            for pair in q.split('&') {
                let mut it = pair.splitn(2, '=');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
        map
    }

    async fn handle_index(&mut self) -> anyhow::Result<()> {
        let port = self.stream.local_addr().map(|a| a.port()).unwrap_or(80);
        let html = self.build_dashboard_html(port);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/html\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            html.len(),
            html
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    fn build_dashboard_html(&self, port: u16) -> String {
        format!(
            r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ZLMediaKit-RS</title>
<script src="https://cdn.jsdelivr.net/npm/hls.js@latest"></script>
<script src="https://cdn.jsdelivr.net/npm/flv.js@latest"></script>
<style>
* {{ margin:0; padding:0; box-sizing:border-box; }}
body {{ font:14px/1.4 monospace; background:#0d1117; color:#c9d1d9; padding:16px; }}
h1 {{ color:#58a6ff; font-size:20px; margin-bottom:12px; }}
h2 {{ color:#8b949e; font-size:14px; text-transform:uppercase; letter-spacing:1px; margin:16px 0 8px; }}
.card {{ background:#161b22; border:1px solid #30363d; border-radius:6px; padding:12px; margin-bottom:12px; }}
table {{ width:100%; border-collapse:collapse; font-size:13px; }}
th,td {{ text-align:left; padding:6px 8px; border-bottom:1px solid #21262d; }}
th {{ color:#8b949e; }}
tr:hover td {{ background:#1c2128; }}
.badge {{ display:inline-block; padding:2px 8px; border-radius:10px; font-size:11px; }}
.badge-green {{ background:#238636; color:#fff; }}
.badge-red {{ background:#da3633; color:#fff; }}
.badge-blue {{ background:#1f6feb; color:#fff; }}
input {{ background:#0d1117; border:1px solid #30363d; color:#c9d1d9; padding:6px 8px; border-radius:4px; font:13px monospace; width:300px; }}
button {{ background:#238636; color:#fff; border:none; padding:6px 14px; border-radius:4px; cursor:pointer; font:13px monospace; }}
button:hover {{ background:#2ea043; }}
button.danger {{ background:#da3633; }}
button.danger:hover {{ background:#f85149; }}
#player-container video {{ width:100%; max-width:640px; background:#000; border-radius:4px; }}
.status-dot {{ display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; }}
.status-dot.on {{ background:#3fb950; }}
.status-dot.off {{ background:#da3633; }}
.toolbar {{ display:flex; gap:8px; align-items:center; flex-wrap:wrap; }}
</style>
</head>
<body>

<h1>ZLMediaKit-RS &middot; Monitoring Dashboard</h1>

<div class="card" style="display:flex; gap:24px; flex-wrap:wrap;">
  <div><span class="status-dot on"></span><strong id="server-name">zlmediakit-rs</strong> <span id="server-version">v0.1.0</span></div>
  <div><strong>Streams:</strong> <span id="stream-count">0</span></div>
  <div><strong>Players:</strong> <span id="player-count">0</span></div>
  <div><button onclick="refreshStreams()" style="font-size:11px; padding:3px 10px;">&#x21bb; Refresh</button></div>
</div>

<div class="card">
  <h2>Active Streams</h2>
  <table><thead><tr>
    <th>App</th><th>Stream</th><th>Status</th><th>Viewers</th><th>Alive</th><th>Video</th><th>Audio</th><th>Actions</th>
  </tr></thead><tbody id="stream-table-body">
    <tr><td colspan="8" style="text-align:center; color:#8b949e;">No active streams</td></tr>
  </tbody></table>
</div>

<div class="card">
  <h2>Player</h2>
  <div class="toolbar">
    <input id="stream-url" type="text" value="/live/test/hls.m3u8" placeholder="Stream URL" />
    <button onclick="playStream()">&#x25b6; Play</button>
    <button class="danger" onclick="stopPlayer()">&#x25a0; Stop</button>
    <select id="player-type" style="background:#0d1117; border:1px solid #30363d; color:#c9d1d9; padding:6px; border-radius:4px;">
      <option value="auto">Auto</option>
      <option value="hls">HLS</option>
      <option value="flv">HTTP-FLV</option>
      <option value="wsflv">WebSocket-FLV</option>
    </select>
  </div>
  <div id="player-container" style="margin-top:8px;display:none;">
    <video id="video-player" controls autoplay muted></video>
    <div id="player-info" style="font-size:12px; color:#8b949e; margin-top:4px;"></div>
  </div>
  <div id="player-error" style="color:#f85149; font-size:13px; margin-top:4px; display:none;"></div>
</div>

<div class="card">
  <h2>Stream URLs</h2>
  <table><tr><td>RTMP</td><td><code>rtmp://host:{0}/live/stream</code></td></tr>
  <tr><td>RTSP</td><td><code>rtsp://host:{0}/live/stream</code></td></tr>
  <tr><td>HTTP-FLV</td><td><code>http://host:{0}/live/stream.flv</code></td></tr>
  <tr><td>HLS</td><td><code>http://host:{0}/live/stream/hls.m3u8</code></td></tr>
  <tr><td>HLS (alt)</td><td><code>http://host:{0}/live/stream.m3u8</code></td></tr>
  <tr><td>WebSocket-FLV</td><td><code>ws://host:{0}/live/stream.flv</code></td></tr>
  <tr><td>API</td><td><code>http://host:{0}/index/api/getMediaList</code></td></tr>
</table>
</div>

<script>
let refreshTimer = null;
let currentPlayer = null;

function refreshStreams() {{
  fetch('/index/api/getMediaList')
    .then(r => r.json())
    .then(data => {{
      const list = data.result || [];
      document.getElementById('stream-count').textContent = list.length;
      const tb = document.getElementById('stream-table-body');
      if (list.length === 0) {{
        tb.innerHTML = '<tr><td colspan="8" style="text-align:center;color:#8b949e;">No active streams</td></tr>';
        document.getElementById('player-count').textContent = '0';
        return;
      }}
      let totalViewers = 0;
      tb.innerHTML = list.map(function(s) {{
        totalViewers += s.readerCount || 0;
        var vtracks = (s.tracks || []).filter(function(t) {{ return t.codec_type === 'video'; }});
        var atracks = (s.tracks || []).filter(function(t) {{ return t.codec_type === 'audio'; }});
        var vinfo = vtracks.map(function(t) {{ return t.codec_id + ' ' + (t.width||'?')+'x'+(t.height||'?'); }}).join(', ') || '-';
        var ainfo = atracks.map(function(t) {{ return t.codec_id+' '+((t.sample_rate||0)/1000).toFixed(1)+'kHz '+((t.channels||0))+'ch'; }}).join(', ') || '-';
        var alive = s.createTime ? Math.floor((Date.now() - s.createTime) / 1000) + 's' : '-';
        var app = s.app || 'live';
        var stream = s.stream || '';
        var streamDisp = s.stream || '-';
        return '<tr>' +
          '<td>' + (s.app||'-') + '</td>' +
          '<td><a href="#" onclick="setStreamUrl(\'' + app + '\',\'' + stream + '\');return false;">' + streamDisp + '</a></td>' +
          '<td><span class="badge badge-green">live</span></td>' +
          '<td>' + (s.readerCount||0) + '</td>' +
          '<td>' + alive + '</td>' +
          '<td style="font-size:11px;">' + vinfo + '</td>' +
          '<td style="font-size:11px;">' + ainfo + '</td>' +
          '<td><button class="danger" style="font-size:11px;padding:2px 6px;" onclick="closeStream(\'' + app + '\',\'' + stream + '\')">X</button></td>' +
          '</tr>';
      }}).join('');
      document.getElementById('player-count').textContent = totalViewers;
    }})
    .catch(() => {{}});
}}

function closeStream(app, stream) {{
  if (!confirm('Close stream: ' + app + '/' + stream + '?')) return;
  fetch('/index/api/closeStream?app='+app+'&stream='+stream+'&vhost=__defaultVhost__')
    .then(r => r.json()).then(() => refreshStreams()).catch(() => {{}});
}}

function setStreamUrl(app, stream) {{
  const sel = document.getElementById('player-type').value;
  if (sel === 'wsflv')
    document.getElementById('stream-url').value = 'ws://' + window.location.host + '/' + app + '/' + stream + '.flv';
  else if (sel === 'hls' || sel === 'auto')
    document.getElementById('stream-url').value = '/' + app + '/' + stream + '/hls.m3u8';
  else
    document.getElementById('stream-url').value = '/' + app + '/' + stream + '.flv';
  playStream();
}}

function playStream() {{
  stopPlayer();
  const url = document.getElementById('stream-url').value.trim();
  if (!url) return;
  const playerType = document.getElementById('player-type').value;

  const container = document.getElementById('player-container');
  const video = document.getElementById('video-player');
  const info = document.getElementById('player-info');
  const errEl = document.getElementById('player-error');
  errEl.style.display = 'none';
  container.style.display = 'block';

  const isAbsolute = url.startsWith('http://') || url.startsWith('https://') || url.startsWith('ws://');
  const fullUrl = isAbsolute ? url : window.location.protocol + '//' + window.location.host + url;

  if (url.endsWith('.m3u8') || playerType === 'hls') {{
    info.textContent = 'HLS: ' + fullUrl;
    if (Hls.isSupported()) {{
      const hls = new Hls();
      hls.loadSource(fullUrl);
      hls.attachMedia(video);
      hls.on(Hls.Events.MANIFEST_PARSED, () => video.play().catch(() => {{}}));
      hls.on(Hls.Events.ERROR, (e, d) => {{
        if (d.fatal) {{ errEl.textContent = 'HLS error: ' + d.type + ' ' + d.details; errEl.style.display = 'block'; }}
      }});
      currentPlayer = hls;
    }} else if (video.canPlayType('application/vnd.apple.mpegurl')) {{
      video.src = fullUrl;
      currentPlayer = video;
    }} else {{
      errEl.textContent = 'HLS not supported in this browser';
      errEl.style.display = 'block';
    }}
  }} else if (url.startsWith('ws://') || url.endsWith('.flv') || playerType === 'flv' || playerType === 'wsflv') {{
    const label = url.startsWith('ws://') ? 'WebSocket-FLV' : 'HTTP-FLV';
    info.textContent = label + ': ' + (url.startsWith('ws://') ? url : fullUrl);
    if (flvjs.isSupported()) {{
      const f = flvjs.createPlayer({{ type: 'flv', url: url.startsWith('ws://') ? url : fullUrl, isLive: true }}, {{ enableWorker: false }});
      f.attachMediaElement(video);
      f.load();
      f.play();
      currentPlayer = f;
    }} else {{
      errEl.textContent = 'FLV not supported in this browser';
      errEl.style.display = 'block';
    }}
  }} else {{
    info.textContent = url;
    video.src = fullUrl;
    currentPlayer = video;
  }}
}}

function stopPlayer() {{
  const video = document.getElementById('video-player');
  if (currentPlayer) {{
    if (currentPlayer.destroy) currentPlayer.destroy();
    else if (currentPlayer.pause) currentPlayer.pause();
    video.src = '';
    currentPlayer = null;
  }}
  document.getElementById('player-container').style.display = 'none';
}}

refreshStreams();
if (refreshTimer) clearInterval(refreshTimer);
refreshTimer = setInterval(refreshStreams, 3000);
</script>
</body>
</html>"##,
            port
        )
    }

    async fn send_400(&mut self) -> anyhow::Result<()> {
        let response = "HTTP/1.1 400 Bad Request\r\n\
            Content-Length: 0\r\n\
            Connection: close\r\n\r\n";
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    async fn send_404(&mut self) -> anyhow::Result<()> {
        let body = "Not Found";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }

    async fn send_401(&mut self) -> anyhow::Result<()> {
        let body = "Unauthorized";
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }
}

/// Background task: subscribes to a MediaSource and generates fMP4 segments
/// every 4 seconds, storing them in DASH_SEGMENTS.
async fn start_dash_task(
    source: Arc<zlmediakit_core::media_source::MediaSource>,
    segs: Arc<RwLock<VecDeque<Fmp4Segment>>>,
) {
    use zlmediakit_core::gop_cache;

    let mut muxer = Fmp4Muxer::new(90000);
    let mut rx = source.subscribe();

    // Replay config frames
    let configs = {
        let cache = source.gop_cache.read().await;
        cache.get_config_frames()
    };
    for frame in &configs {
        muxer.push_frame(frame);
    }

    // Replay GOP cache
    let gop = {
        let cache = source.gop_cache.read().await;
        cache.get_latest_gop_frames()
    };
    for frame in &gop {
        if !gop_cache::is_config_frame(frame) {
            muxer.push_frame(frame);
        }
    }

    // Flush initial segment
    if let Some(seg) = muxer.flush_segment() {
        let mut guard = segs.write().await;
        guard.push_back(seg);
        while guard.len() > 10 {
            guard.pop_front();
        }
    }

    let mut last_flush = tokio::time::Instant::now();
    let flush_interval = tokio::time::Duration::from_secs(4);

    loop {
        tokio::select! {
            frame = rx.recv() => {
                match frame {
                    Ok(f) => {
                        if !gop_cache::is_config_frame(&f) {
                            muxer.push_frame(&f);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep_until(last_flush + flush_interval) => {
                if let Some(seg) = muxer.flush_segment() {
                    let mut guard = segs.write().await;
                    guard.push_back(seg);
                    while guard.len() > 10 {
                        guard.pop_front();
                    }
                }
                last_flush = tokio::time::Instant::now();
            }
        }
    }
}

impl HttpSession {
    /// Serves a snapshot (JPEG) of the given stream by extracting the latest
    /// H.264 key frame from the GOP cache, converting AVCC → AnnexB and piping
    /// it through `ffmpeg` to produce a single MJPEG frame.
    ///
    /// Query params: `vhost`, `app`, `stream` (and optional `.jpeg` suffix in
    /// `stream` is stripped).
    async fn handle_get_snap(&mut self, path: &str) -> anyhow::Result<()> {
        let q = Self::parse_query(path);
        let vhost = q
            .get("vhost")
            .map(|s| s.as_str())
            .unwrap_or("__defaultVhost__");
        let app = q.get("app").map(|s| s.as_str()).unwrap_or("live");
        let stream = q
            .get("stream")
            .map(|s| s.as_str())
            .unwrap_or("")
            .trim_end_matches(".jpeg")
            .trim_end_matches(".jpg");

        if stream.is_empty() {
            self.send_json(&serde_json::json!({"code": -400, "msg": "missing stream param"}))
                .await?;
            return Ok(());
        }

        let source = match self.source_manager.get(vhost, app, stream) {
            Some(s) => s,
            None => {
                self.send_json(&serde_json::json!({"code": -404, "msg": "stream not found"}))
                    .await?;
                return Ok(());
            }
        };

        let config_frames = source.get_cached_config_frames().await;
        let gop_frames = source.get_latest_gop_frames().await;

        let sps_pps = config_frames.iter().find(|f| {
            matches!(f.codec, zlmediakit_core::media_frame::CodecId::H264)
                && f.config_frame
                && matches!(f.frame_type, zlmediakit_core::media_frame::FrameType::Video)
        });
        let key = gop_frames.iter().find(|f| {
            matches!(f.codec, zlmediakit_core::media_frame::CodecId::H264)
                && f.key_frame
                && matches!(f.frame_type, zlmediakit_core::media_frame::FrameType::Video)
        });

        let (sps_pps, key) = match (sps_pps, key) {
            (Some(c), Some(k)) => (c, k),
            _ => {
                self.send_json(&serde_json::json!({
                    "code": -404,
                    "msg": "no H.264 key frame available yet"
                }))
                .await?;
                return Ok(());
            }
        };

        let mut annexb: Vec<u8> = Vec::new();
        annexb.extend_from_slice(&h264_to_annexb(&sps_pps.data));
        annexb.extend_from_slice(&h264_to_annexb(&key.data));

        let tmp = std::env::temp_dir().join(format!(
            "zlm_snap_{}_{}_{}_{}.h264",
            vhost,
            app,
            stream,
            std::process::id()
        ));
        tokio::fs::write(&tmp, &annexb).await?;

        let out = tokio::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-i",
                tmp.to_str().unwrap_or("/dev/null"),
                "-frames:v",
                "1",
                "-f",
                "mjpeg",
                "-",
            ])
            .output()
            .await;

        let _ = tokio::fs::remove_file(&tmp).await;

        match out {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                let body = o.stdout;
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: image/jpeg\r\n\
                     Content-Length: {}\r\n\
                     Access-Control-Allow-Origin: *\r\n\
                     Cache-Control: no-cache\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                self.stream.write_all(response.as_bytes()).await?;
                self.stream.write_all(&body).await?;
            }
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr);
                error!("ffmpeg getSnap failed: {}", msg);
                self.send_json(&serde_json::json!({"code": -500, "msg": "ffmpeg failed"}))
                    .await?;
            }
            Err(e) => {
                error!("could not spawn ffmpeg for getSnap: {}", e);
                self.send_json(&serde_json::json!({"code": -500, "msg": "ffmpeg not available"}))
                    .await?;
            }
        }
        Ok(())
    }
}

/// Converts an H.264 NALU stream from AVCC (length-prefixed) to AnnexB
/// (start-code prefixed). If the data already starts with a start code, it is
/// returned unchanged.
fn h264_to_annexb(data: &[u8]) -> Vec<u8> {
    if data.len() >= 4 && data[0..4] == [0, 0, 0, 1] {
        return data.to_vec();
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len() + 16);
    let mut i = 0;
    while i + 4 <= data.len() {
        let nalu_len =
            u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if i + nalu_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + nalu_len]);
        i += nalu_len;
    }
    out
}

/// Splits an `ip:port` (or `[ipv6]:port`) socket address string into its parts.
/// Returns `("", 0)` for unparseable input.
fn split_addr(addr: &str) -> (String, u16) {
    if addr.is_empty() {
        return (String::new(), 0);
    }
    if let Some(rest) = addr.strip_prefix('[') {
        // [ipv6]:port
        if let Some((ip, port)) = rest.split_once("]:") {
            return (format!("[{}]", ip), port.parse().unwrap_or(0));
        }
        return (addr.to_string(), 0);
    }
    match addr.rsplit_once(':') {
        Some((ip, port)) => (ip.to_string(), port.parse().unwrap_or(0)),
        None => (addr.to_string(), 0),
    }
}

/// Lists MP4 record files under `base` (record_root/app/stream), optionally
/// filtered by `period` (a date prefix like `20240101` or `2024-01-01`).
fn list_record_files(
    base: &Path,
    period: &str,
    vhost: &str,
    app: &str,
    stream: &str,
) -> Vec<serde_json::Value> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return files;
    };
    let period = period.replace('-', "");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "mp4").unwrap_or(false) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !period.is_empty() && !name.starts_with(&period) {
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let start_time = parse_record_start(&name);
            files.push(serde_json::json!({
                "app": app,
                "stream": stream,
                "vhost": vhost,
                "file_name": name,
                "file_path": path.to_string_lossy(),
                "file_size": size,
                "startTime": start_time,
                "time_len": 0,
                "url": format!("/record/{}/{}/{}", app, stream, name),
            }));
        }
    }
    files
}

/// Parses a `YYYYMMDD_HHMMSS.mp4` record file name into a unix timestamp
/// (milliseconds). Returns 0 when it cannot be parsed.
fn parse_record_start(name: &str) -> i64 {
    let stem = name.trim_end_matches(".mp4");
    let (date, time) = stem.split_once('_').unwrap_or((stem, ""));
    let ds = date
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    let ts = time
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    if ds.len() < 8 || (ts.len() < 4 && !ts.is_empty()) {
        return 0;
    }
    let y: i32 = ds[0..4].parse().unwrap_or(0);
    let mo: u32 = ds[4..6].parse().unwrap_or(1);
    let d: u32 = ds[6..8].parse().unwrap_or(1);
    let (h, mi, s): (u32, u32, u32) = if ts.is_empty() {
        (0, 0, 0)
    } else {
        (
            ts[0..2].parse().unwrap_or(0),
            ts[2..4].parse().unwrap_or(0),
            if ts.len() >= 6 {
                ts[4..6].parse().unwrap_or(0)
            } else {
                0
            },
        )
    };
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(y, mo, d, h, mi, s)
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}
