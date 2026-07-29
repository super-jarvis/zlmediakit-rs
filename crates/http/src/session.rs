use bytes::BytesMut;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{debug, error, warn};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_core::transport::TransportStream;
use zlmediakit_hls::muxer::{self, HlsSegment};

use zlmediakit_flv::FlvMuxer;

pub static HLS_SEGMENTS: once_cell::sync::Lazy<DashMap<String, Arc<RwLock<VecDeque<HlsSegment>>>>> =
    once_cell::sync::Lazy::new(DashMap::new);

pub struct HttpSession {
    stream: TransportStream,
    peer_addr: String,
    source_manager: Arc<MediaSourceManager>,
    auth: Arc<StreamAuth>,
    recorder: Arc<RecorderControl>,
    proxy: Arc<StreamProxyControl>,
    buffer: BytesMut,
}

impl HttpSession {
    pub fn new(
        stream: TransportStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
        auth: Arc<StreamAuth>,
        recorder: Arc<RecorderControl>,
        proxy: Arc<StreamProxyControl>,
    ) -> Self {
        Self {
            stream,
            peer_addr,
            source_manager,
            auth,
            recorder,
            proxy,
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
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

        let method = parts[0];
        let path = parts[1];

        debug!("HTTP {} {} from {}", method, path, self.peer_addr);

        // Route on the path without any query string; the handlers still
        // receive the full `path` so they can extract `?sign=` and other params.
        let route = path.split_once('?').map(|(p, _)| p).unwrap_or(path);

        if route.starts_with("/index/api/") {
            self.handle_api(path).await?;
        } else if route.ends_with(".flv") {
            self.handle_flv_stream(path).await?;
        } else if route.ends_with(".m3u8") {
            self.handle_hls_playlist(path).await?;
        } else if route.ends_with(".ts") {
            self.handle_hls_segment(path).await?;
        } else if route == "/" || route == "/index.html" {
            self.handle_index().await?;
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

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HTTP-FLV play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, &stream_name);

        match source {
            Some(source) => {
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
                    let tag = muxer.write_tag(frame);
                    self.stream.write_all(&tag).await?;
                }

                let mut rx = source.subscribe();

                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                                continue;
                            }
                            let tag = muxer.write_tag(&frame);
                            self.stream.write_all(&tag).await?;
                        }
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

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("HLS playlist play rejected (auth): {}/{}", app, stream_name);
            return self.send_401().await;
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, &stream_name);

        if let Some(source) = source {
            let key = source.url();
            let segments = HLS_SEGMENTS.entry(key.clone()).or_insert_with(|| {
                let segs: Arc<RwLock<VecDeque<HlsSegment>>> =
                    Arc::new(RwLock::new(VecDeque::new()));
                let segs_clone = segs.clone();
                let source_clone = source.clone();
                muxer::start_hls_task(source_clone, segs_clone);
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

    async fn handle_api(&mut self, path: &str) -> anyhow::Result<()> {
        let api = path.split_once('?').map(|(a, _)| a).unwrap_or(path);
        let api = api.trim_start_matches("/index/api/");

        match api {
            "getMediaList" => {
                let sources = self.source_manager.list();
                let mut list = Vec::new();
                for source in &sources {
                    let reader = source.subscriber_count().await;
                    let info = source.info.read().await;
                    let tracks: Vec<serde_json::Value> =
                        info.tracks.iter().map(Self::track_to_json).collect();
                    drop(info);
                    list.push(serde_json::json!({
                        "app": source.app,
                        "stream": source.stream,
                        "vhost": source.vhost,
                        "url": source.url(),
                        "readerCount": reader,
                        "totalReaderCount": reader,
                        "createTime": source.created_at.timestamp_millis(),
                        "tracks": tracks,
                    }));
                }
                self.send_json(&serde_json::json!({"code": 0, "result": list}))
                    .await?;
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
                self.send_json(&serde_json::json!({
                    "code": 0,
                    "server": "zlmediakit-rs",
                    "version": env!("CARGO_PKG_VERSION"),
                    "api_version": "1.0",
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

    /// Decides which container formats to record from the `type` query param.
    /// ZLMediaKit uses `type`: `hls`/`flv`/`mp4` (or `all`). Defaults to HLS.
    fn api_record_types(path: &str) -> (bool, bool, bool) {
        let q = Self::parse_query(path);
        let t = q
            .get("type")
            .map(|s| s.as_str())
            .unwrap_or("hls")
            .to_lowercase();
        let hls = t == "hls" || t == "all";
        let flv = t == "flv" || t == "all";
        let mp4 = t == "mp4" || t == "all";
        (hls, flv, mp4)
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
  if (sel === 'hls' || sel === 'auto')
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

  const isAbsolute = url.startsWith('http://') || url.startsWith('https://');
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
  }} else if (url.endsWith('.flv') || playerType === 'flv') {{
    info.textContent = 'HTTP-FLV: ' + fullUrl;
    if (flvjs.isSupported()) {{
      const f = flvjs.createPlayer({{ type: 'flv', url: fullUrl }}, {{ enableWorker: false }});
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
