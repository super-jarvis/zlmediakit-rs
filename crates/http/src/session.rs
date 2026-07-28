use bytes::BytesMut;
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tracing::{debug, error};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_hls::muxer::{self, HlsSegment};

use zlmediakit_flv::FlvMuxer;

pub static HLS_SEGMENTS: once_cell::sync::Lazy<DashMap<String, Arc<RwLock<VecDeque<HlsSegment>>>>> =
    once_cell::sync::Lazy::new(DashMap::new);

pub struct HttpSession {
    stream: TcpStream,
    peer_addr: String,
    source_manager: Arc<MediaSourceManager>,
    buffer: BytesMut,
}

impl HttpSession {
    pub fn new(
        stream: TcpStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
    ) -> Self {
        Self {
            stream,
            peer_addr,
            source_manager,
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

        if path.starts_with("/index/api/") {
            self.handle_api(path).await?;
        } else if path.ends_with(".flv") {
            self.handle_flv_stream(path).await?;
        } else if path.ends_with(".m3u8") {
            self.handle_hls_playlist(path).await?;
        } else if path.ends_with(".ts") {
            self.handle_hls_segment(path).await?;
        } else if path == "/" || path == "/index.html" {
            self.handle_index().await?;
        } else {
            self.send_404().await?;
        }

        Ok(())
    }

    async fn handle_flv_stream(&mut self, path: &str) -> anyhow::Result<()> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        let app = parts.first().map_or("live", |v| v).to_string();
        let stream_name = parts.get(1)
            .and_then(|s| s.strip_suffix(".flv"))
            .map_or("stream", |v| v)
            .to_string();

        let source = self.source_manager.get("__defaultVhost__", &app, &stream_name);

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
                if let Some(zlmediakit_core::media_frame::TrackInfo::Video(ref v)) = info.tracks.first() {
                    let meta = muxer.write_metadata(v.width, v.height, v.fps, 44100);
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
        let stream_path = path.trim_start_matches('/').trim_end_matches("hls.m3u8").trim_end_matches('/');
        let parts: Vec<&str> = stream_path.splitn(2, '/').collect();
        let app = parts.first().unwrap_or(&"live");
        let stream_name = parts.get(1).unwrap_or(&"stream");

        let source = self.source_manager.get("__defaultVhost__", app, stream_name);

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
                m3u8.push_str(&format!(
                    "#EXT-X-MEDIA-SEQUENCE:{}\n",
                    first_idx
                ));
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
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() < 3 {
            self.send_404().await?;
            return Ok(());
        }

        let app = parts[0];
        let stream_name = parts[1];
        let file_name = parts[2];

        let idx: u32 = if let Some(name) = file_name.strip_suffix(".ts") {
            name.parse().unwrap_or(u32::MAX)
        } else {
            u32::MAX
        };

        if idx == u32::MAX {
            self.send_404().await?;
            return Ok(());
        }

        let source = self.source_manager.get("__defaultVhost__", app, stream_name);

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
        let api = path.trim_start_matches("/index/api/");

        match api {
            "getMediaList" => {
                let sources = self.source_manager.list();
                let mut list = Vec::new();
                for source in &sources {
                    list.push(serde_json::json!({
                        "app": source.app,
                        "stream": source.stream,
                        "vhost": source.vhost,
                        "url": source.url(),
                    }));
                }

                let body = serde_json::to_string_pretty(&serde_json::json!({
                    "code": 0,
                    "result": list,
                }))?;

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                self.stream.write_all(response.as_bytes()).await?;
            }
            "getServerConfig" => {
                let body = serde_json::to_string_pretty(&serde_json::json!({
                    "code": 0,
                    "server": "zlmediakit-rs",
                    "version": "0.1.0",
                }))?;

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                self.stream.write_all(response.as_bytes()).await?;
            }
            _ => {
                self.send_404().await?;
            }
        }

        Ok(())
    }

    async fn handle_index(&mut self) -> anyhow::Result<()> {
        let html = r#"<!DOCTYPE html>
<html>
<head>
    <title>ZLMediaKit-RS</title>
    <style>
        body { font-family: monospace; padding: 20px; background: #1a1a2e; color: #e0e0e0; }
        h1 { color: #0f3460; }
        .card { background: #16213e; padding: 15px; margin: 10px 0; border-radius: 5px; }
        a { color: #00b4d8; }
    </style>
</head>
<body>
    <h1>ZLMediaKit-RS Streaming Server</h1>
    <div class="card">
        <h2>Supported Protocols</h2>
        <ul>
            <li>RTMP - Port 1935</li>
            <li>RTSP - Port 554</li>
            <li>HTTP-FLV - Port 80</li>
            <li>HLS - Port 80</li>
        </ul>
    </div>
    <div class="card">
        <h2>API Endpoints</h2>
        <ul>
            <li><a href="/index/api/getMediaList">/index/api/getMediaList</a> - List media streams</li>
            <li><a href="/index/api/getServerConfig">/index/api/getServerConfig</a> - Server config</li>
        </ul>
    </div>
    <div class="card">
        <h2>Stream URLs</h2>
        <p>RTMP: rtmp://host/live/stream</p>
        <p>RTSP: rtsp://host/live/stream</p>
        <p>HTTP-FLV: http://host/live/stream.flv</p>
        <p>HLS: http://host/live/stream.m3u8</p>
    </div>
</body>
</html>"#;

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
}
