//! Native HTTP media pull client used by stream proxies.
//!
//! The implementation intentionally stays independent of the HTTP server
//! session type: it speaks HTTP/1.1 over Tokio, supports fixed-length,
//! connection-close and chunked bodies, and republishes HTTP-FLV, HTTP-TS and
//! MPEG-TS HLS segments through the shared `MediaSourceManager`.

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Buf, BytesMut};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info};
use zlmediakit_core::media_frame::{AudioInfo, FrameType, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};
use zlmediakit_flv::demuxer::FlvDemuxer;
use zlmediakit_mp4::Fmp4Demuxer;
use zlmediakit_srt::TsStreamDemuxer;

const MAX_HEADER_SIZE: usize = 64 * 1024;
const MAX_PLAYLIST_SIZE: usize = 2 * 1024 * 1024;
const MAX_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct HttpUrl {
    scheme: &'static str,
    host: String,
    authority: String,
    port: u16,
    path: String,
}

impl HttpUrl {
    fn parse(url: &str) -> Result<Self> {
        let (scheme, rest, default_port) = if let Some(rest) = url.strip_prefix("http://") {
            ("http", rest, 80)
        } else if let Some(rest) = url.strip_prefix("https://") {
            ("https", rest, 443)
        } else {
            bail!("native HTTP pull requires an http:// or https:// URL");
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            bail!("HTTP URL is missing a host");
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let end = bracketed
                .find(']')
                .ok_or_else(|| anyhow!("invalid bracketed IPv6 HTTP host"))?;
            let host = bracketed[..end].to_string();
            let suffix = &bracketed[end + 1..];
            let port = suffix
                .strip_prefix(':')
                .map(str::parse)
                .transpose()
                .context("invalid HTTP URL port")?
                .unwrap_or(default_port);
            (host, port)
        } else if authority.matches(':').count() == 1 {
            let (host, port) = authority.rsplit_once(':').expect("count checked");
            (
                host.to_string(),
                port.parse().context("invalid HTTP URL port")?,
            )
        } else {
            (authority.to_string(), default_port)
        };
        Ok(Self {
            scheme,
            host,
            authority: authority.to_string(),
            port,
            path: path.to_string(),
        })
    }

    fn resolve(&self, reference: &str) -> Result<String> {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Ok(reference.to_string());
        }
        let reference = reference.split('#').next().unwrap_or(reference);
        let raw_path = if reference.starts_with('/') {
            reference.to_string()
        } else {
            let base = self.path.split('?').next().unwrap_or(&self.path);
            let directory = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            format!("{directory}/{reference}")
        };
        let (path, query) = raw_path
            .split_once('?')
            .map_or((raw_path.as_str(), None), |(path, query)| {
                (path, Some(query))
            });
        let mut normalized = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    normalized.pop();
                }
                other => normalized.push(other),
            }
        }
        let mut result = format!(
            "{}://{}/{}",
            self.scheme,
            self.authority,
            normalized.join("/")
        );
        if let Some(query) = query {
            result.push('?');
            result.push_str(query);
        }
        Ok(result)
    }

    fn as_url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.authority, self.path)
    }
}

struct HttpResponse {
    url: HttpUrl,
    status: u16,
    headers: HashMap<String, String>,
    body: HttpBody,
}

struct HttpBody {
    stream: Box<dyn AsyncStream>,
    buffer: BytesMut,
    chunked: bool,
    content_remaining: Option<usize>,
    finished: bool,
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

impl HttpBody {
    async fn fill(&mut self) -> Result<bool> {
        let mut chunk = [0u8; 32 * 1024];
        let count = self.stream.read(&mut chunk).await?;
        if count == 0 {
            return Ok(false);
        }
        self.buffer.extend_from_slice(&chunk[..count]);
        Ok(true)
    }

    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }
        if self.chunked {
            let line_end = loop {
                if let Some(position) = find_bytes(&self.buffer, b"\r\n") {
                    break position;
                }
                if !self.fill().await? {
                    bail!("truncated HTTP chunk header");
                }
            };
            let size_line = std::str::from_utf8(&self.buffer[..line_end])?;
            let size =
                usize::from_str_radix(size_line.split(';').next().unwrap_or(size_line).trim(), 16)
                    .context("invalid HTTP chunk size")?;
            self.buffer.advance(line_end + 2);
            if size == 0 {
                self.finished = true;
                return Ok(None);
            }
            while self.buffer.len() < size + 2 {
                if !self.fill().await? {
                    bail!("truncated HTTP chunk body");
                }
            }
            let data = self.buffer.split_to(size).to_vec();
            if &self.buffer[..2] != b"\r\n" {
                bail!("invalid HTTP chunk terminator");
            }
            self.buffer.advance(2);
            return Ok(Some(data));
        }

        if self.buffer.is_empty() && !self.fill().await? {
            self.finished = true;
            return Ok(None);
        }
        let count = match self.content_remaining {
            Some(0) => {
                self.finished = true;
                return Ok(None);
            }
            Some(remaining) => remaining.min(self.buffer.len()),
            None => self.buffer.len(),
        };
        let data = self.buffer.split_to(count).to_vec();
        if let Some(remaining) = &mut self.content_remaining {
            *remaining -= count;
        }
        Ok(Some(data))
    }

    async fn read_to_end(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        while let Some(chunk) = self.read_chunk().await? {
            if output.len() + chunk.len() > limit {
                bail!("HTTP response body exceeds {limit} bytes");
            }
            output.extend_from_slice(&chunk);
        }
        Ok(output)
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn open_response(url: &str) -> Result<HttpResponse> {
    let mut current = url.to_string();
    for _ in 0..=5 {
        let parsed = HttpUrl::parse(&current)?;
        let tcp_stream = tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect((parsed.host.as_str(), parsed.port)),
        )
        .await
        .context("HTTP connect timed out")?
        .with_context(|| format!("failed to connect {}:{}", parsed.host, parsed.port))?;
        let mut stream: Box<dyn AsyncStream> = if parsed.scheme == "https" {
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name =
                ServerName::try_from(parsed.host.clone()).context("invalid HTTPS server name")?;
            let tls = tokio::time::timeout(
                Duration::from_secs(10),
                TlsConnector::from(Arc::new(config)).connect(server_name, tcp_stream),
            )
            .await
            .context("HTTPS handshake timed out")?
            .context("HTTPS certificate verification or handshake failed")?;
            Box::new(tls)
        } else {
            Box::new(tcp_stream)
        };
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: zlmediakit-rs/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            parsed.path, parsed.authority
        );
        stream.write_all(request.as_bytes()).await?;

        let mut buffer = BytesMut::with_capacity(8192);
        let header_end = loop {
            if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
                break position;
            }
            if buffer.len() >= MAX_HEADER_SIZE {
                bail!("HTTP response header exceeds {MAX_HEADER_SIZE} bytes");
            }
            let mut chunk = [0u8; 8192];
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                bail!("connection closed before HTTP response headers");
            }
            buffer.extend_from_slice(&chunk[..count]);
        };
        let raw_headers = buffer.split_to(header_end + 4);
        let header_text = std::str::from_utf8(&raw_headers[..header_end])?;
        let mut lines = header_text.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("invalid HTTP status line"))?;
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = headers
                .get("location")
                .ok_or_else(|| anyhow!("HTTP redirect is missing Location"))?;
            current = parsed.resolve(location)?;
            continue;
        }
        if !(200..300).contains(&status) {
            bail!("HTTP upstream returned status {status}");
        }
        let chunked = headers
            .get("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        let content_remaining = headers
            .get("content-length")
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("invalid HTTP Content-Length")?;
        return Ok(HttpResponse {
            url: parsed,
            status,
            headers,
            body: HttpBody {
                stream,
                buffer,
                chunked,
                content_remaining,
                finished: false,
            },
        });
    }
    bail!("too many HTTP redirects")
}

/// Pull an HTTP-FLV, HTTP-TS or MPEG-TS HLS URL into a local media source.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    info!("HTTP pull: {url} -> {vhost}/{app}/{stream_name}");
    let response = open_response(url).await?;
    debug!(status = response.status, "HTTP pull response opened");
    let content_type = response
        .headers
        .get("content-type")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let path = response.url.path.to_ascii_lowercase();
    if path
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with(".m3u8"))
        || content_type.contains("mpegurl")
    {
        return pull_hls(
            response,
            url,
            vhost,
            app,
            stream_name,
            source_manager,
            stop,
            stopped,
        )
        .await;
    }
    let source = source_manager.get_or_create(vhost, app, stream_name);
    if path
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with(".flv"))
        || content_type.contains("flv")
    {
        pull_flv(response.body, source, stop, stopped).await
    } else if path
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with(".ts"))
        || content_type.contains("mp2t")
        || content_type.contains("mpeg-ts")
        || content_type.contains("octet-stream")
    {
        pull_ts(response.body, source, stop, stopped).await
    } else {
        bail!("unsupported HTTP media content type: {content_type}")
    }
}

async fn register_track(source: &MediaSource, frame: &zlmediakit_core::MediaFrame) {
    let mut info = source.info.write().await;
    let exists = info.tracks.iter().any(|track| match track {
        TrackInfo::Video(video) => {
            frame.frame_type == FrameType::Video && video.codec == frame.codec
        }
        TrackInfo::Audio(audio) => {
            frame.frame_type == FrameType::Audio && audio.codec == frame.codec
        }
    });
    if exists {
        return;
    }
    match frame.frame_type {
        FrameType::Video => info.tracks.push(TrackInfo::Video(VideoInfo {
            codec: frame.codec,
            width: 0,
            height: 0,
            fps: 0.0,
            key_frame: frame.key_frame,
        })),
        FrameType::Audio => info.tracks.push(TrackInfo::Audio(AudioInfo {
            codec: frame.codec,
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
        })),
        FrameType::Metadata => {}
    }
}

async fn pull_flv(
    mut body: HttpBody,
    source: Arc<MediaSource>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let mut demuxer = FlvDemuxer::new();
    let mut header_parsed = false;
    loop {
        let chunk = tokio::select! {
            _ = stop.notified() => break,
            chunk = body.read_chunk() => chunk?,
        };
        let Some(chunk) = chunk else { break };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        demuxer.feed(&chunk);
        if !header_parsed {
            header_parsed = demuxer.parse_header().is_some();
            if !header_parsed {
                continue;
            }
        }
        while let Some(frame) = demuxer.parse_tag() {
            register_track(&source, &frame).await;
            source.publish_and_cache(frame).await;
        }
    }
    if !header_parsed {
        bail!("HTTP-FLV upstream ended before a valid FLV header");
    }
    Ok(())
}

async fn pull_ts(
    mut body: HttpBody,
    source: Arc<MediaSource>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let mut demuxer = TsStreamDemuxer::new();
    loop {
        let chunk = tokio::select! {
            _ = stop.notified() => break,
            chunk = body.read_chunk() => chunk?,
        };
        let Some(chunk) = chunk else { break };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        demuxer.feed(&chunk, &source).await;
    }
    demuxer.flush(&source).await;
    Ok(())
}

struct PlaylistEntries {
    segments: Vec<String>,
    variant: Option<String>,
    end_list: bool,
    init_map: Option<String>,
}

fn playlist_entries(data: &[u8]) -> Result<PlaylistEntries> {
    let text = std::str::from_utf8(data).context("HLS playlist is not UTF-8")?;
    let mut segments = Vec::new();
    let mut variant = None;
    let mut expect_variant = false;
    let mut init_map = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            expect_variant = true;
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            init_map = attributes.split(',').find_map(|attribute| {
                attribute
                    .trim()
                    .strip_prefix("URI=")
                    .map(|value| value.trim_matches('"').to_string())
            });
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if expect_variant && variant.is_none() {
            variant = Some(line.to_string());
            expect_variant = false;
        } else {
            segments.push(line.to_string());
        }
    }
    Ok(PlaylistEntries {
        segments,
        variant,
        end_list: text.lines().any(|line| line.trim() == "#EXT-X-ENDLIST"),
        init_map,
    })
}

#[allow(clippy::too_many_arguments)]
async fn pull_hls(
    initial_response: HttpResponse,
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let source = source_manager.get_or_create(vhost, app, stream_name);
    let mut playlist_url = url.to_string();
    let mut seen = HashSet::new();
    let mut demuxer = TsStreamDemuxer::new();
    let mut fmp4_demuxer = None;
    let mut current_map_url = None;
    let mut next_response = Some(initial_response);
    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let response = match next_response.take() {
            Some(response) => response,
            None => open_response(&playlist_url).await?,
        };
        let base = response.url.clone();
        playlist_url = base.as_url();
        let playlist = response.body.read_to_end(MAX_PLAYLIST_SIZE).await?;
        let entries = playlist_entries(&playlist)?;
        if let Some(variant) = entries.variant {
            playlist_url = base.resolve(&variant)?;
            continue;
        }
        if let Some(init_map) = entries.init_map {
            let map_url = base.resolve(&init_map)?;
            if current_map_url.as_deref() != Some(map_url.as_str()) {
                let response = open_response(&map_url).await?;
                let data = response.body.read_to_end(MAX_SEGMENT_SIZE).await?;
                let parsed = Fmp4Demuxer::from_init_segment(&data)?;
                source.info.write().await.tracks = parsed.track_info();
                for frame in parsed.config_frames() {
                    source.publish_and_cache(frame).await;
                }
                current_map_url = Some(map_url);
                fmp4_demuxer = Some(parsed);
            }
        }
        for segment in entries.segments {
            let segment_url = base.resolve(&segment)?;
            if !seen.insert(segment_url.clone()) {
                continue;
            }
            let response = open_response(&segment_url).await?;
            let data = response.body.read_to_end(MAX_SEGMENT_SIZE).await?;
            if let Some(fmp4) = &fmp4_demuxer {
                for frame in fmp4.demux_fragment(&data)? {
                    source.publish_and_cache(frame).await;
                }
            } else {
                demuxer.feed(&data, &source).await;
            }
        }
        if entries.end_list {
            break;
        }
        if seen.len() > 4096 {
            seen.clear();
        }
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    demuxer.flush(&source).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use zlmediakit_core::media_frame::{CodecId, MediaFrame};
    use zlmediakit_core::rtp::{RtpDataType, RtpSendRequest, RtpSenderManager};
    use zlmediakit_hls::TsLiveMuxer;
    use zlmediakit_mp4::Fmp4Muxer;

    fn flv_tag(tag_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut tag = vec![tag_type];
        let size = payload.len() as u32;
        tag.extend_from_slice(&[(size >> 16) as u8, (size >> 8) as u8, size as u8]);
        tag.extend_from_slice(&[0; 7]);
        tag.extend_from_slice(payload);
        tag.extend_from_slice(&(11 + size).to_be_bytes());
        tag
    }

    fn test_ts_stream() -> Vec<u8> {
        let mut muxer = TsLiveMuxer::new();
        let mut config = MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::from_static(&[
                0x17, 0, 0, 0, 0, 0x67, 0x42, 0, 0x1f, 0x68, 0xce, 0x3c, 0x80,
            ]),
            false,
        );
        config.config_frame = true;
        let mut avcc = vec![0x17, 1, 0, 0, 0];
        avcc.extend_from_slice(&4u32.to_be_bytes());
        avcc.extend_from_slice(&[0x65, 1, 2, 3]);
        let key = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, Bytes::from(avcc), true);
        let mut next = vec![0x27, 1, 0, 0, 0];
        next.extend_from_slice(&4u32.to_be_bytes());
        next.extend_from_slice(&[0x41, 4, 5, 6]);
        let next = MediaFrame::new_video(0, CodecId::H264, 40, 40, 40, Bytes::from(next), false);
        let mut output = muxer.push_frame(&config);
        output.extend_from_slice(&muxer.push_frame(&key));
        output.extend_from_slice(&muxer.push_frame(&next));
        output
    }

    fn test_fmp4_stream() -> (Vec<u8>, Vec<u8>) {
        let mut muxer = Fmp4Muxer::new(90_000);
        let mut config = MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            Bytes::from_static(&[
                0x17, 0, 0, 0, 0, 0x01, 0x64, 0, 0x0a, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x0a, 1, 0,
                4, 0x68, 0xee, 0x3c, 0x80,
            ]),
            false,
        )
        .with_payload_format(zlmediakit_core::PayloadFormat::Flv);
        config.config_frame = true;
        muxer.push_frame(&config);
        muxer.push_frame(
            &MediaFrame::new_video(
                0,
                CodecId::H264,
                0,
                0,
                0,
                Bytes::from_static(&[0, 0, 0, 1, 0x65, 9, 8, 7]),
                true,
            )
            .with_payload_format(zlmediakit_core::PayloadFormat::AnnexB),
        );
        (
            muxer.init_segment().data.to_vec(),
            muxer.flush_segment().unwrap().data.to_vec(),
        )
    }

    async fn serve_once(listener: &TcpListener, content_type: &str, body: &[u8]) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
    }

    async fn assert_rtp_relay(manager: Arc<MediaSourceManager>, stream: &str, ssrc: u32) {
        let destination = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination_port = destination.local_addr().unwrap().port();
        let sender = RtpSenderManager::new(manager);
        sender
            .start(RtpSendRequest {
                vhost: "__defaultVhost__".to_string(),
                app: "live".to_string(),
                stream: stream.to_string(),
                ssrc,
                dst_host: "127.0.0.1".to_string(),
                dst_port: destination_port,
                src_port: 0,
                payload_type: 96,
                is_udp: true,
                data_type: RtpDataType::Es,
                only_audio: false,
                passive: false,
                ssrc_multi_send: false,
            })
            .await
            .unwrap();
        let mut packet = [0u8; 1500];
        let size = tokio::time::timeout(Duration::from_secs(2), destination.recv(&mut packet))
            .await
            .expect("timed out waiting for RTP relay")
            .unwrap();
        assert!(size > 12);
        assert_eq!(packet[1] & 0x7f, 96);
        assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), ssrc);
        assert_eq!(
            sender.stop("__defaultVhost__", "live", stream, Some(ssrc)),
            1
        );
    }

    #[test]
    fn resolves_hls_relative_paths() {
        let base = HttpUrl::parse("http://127.0.0.1:8080/live/sub/index.m3u8?x=1").unwrap();
        assert_eq!(
            base.resolve("../seg/001.ts?token=a").unwrap(),
            "http://127.0.0.1:8080/live/seg/001.ts?token=a"
        );
        assert_eq!(
            base.resolve("/root/master.m3u8").unwrap(),
            "http://127.0.0.1:8080/root/master.m3u8"
        );
    }

    #[tokio::test]
    async fn http_flv_pull_republishes_media_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mut flv = b"FLV\x01\x01\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
            flv.extend_from_slice(&flv_tag(0x09, &[0x17, 1, 0, 0, 0, 0, 0, 0, 1, 0x65]));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nContent-Length: {}\r\n\r\n",
                flv.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&flv).await.unwrap();
        });

        let manager = Arc::new(MediaSourceManager::new(None));
        let task_manager = manager.clone();
        let task = tokio::spawn(async move {
            start(
                &format!("http://127.0.0.1:{port}/live/test.flv"),
                "__defaultVhost__",
                "live",
                "http_flv",
                task_manager,
                Arc::new(Notify::new()),
                Arc::new(AtomicBool::new(false)),
            )
            .await
        });
        task.await.unwrap().unwrap();
        let source = manager
            .get("__defaultVhost__", "live", "http_flv")
            .expect("HTTP pull did not create the local media source");
        let frame = source
            .get_latest_gop_frames()
            .await
            .into_iter()
            .next()
            .expect("HTTP pull did not cache a media frame");
        assert_eq!(frame.frame_type, FrameType::Video);
        assert!(frame.key_frame);
        assert_eq!(frame.payload_format, zlmediakit_core::PayloadFormat::Flv);
        assert_rtp_relay(manager, "http_flv", 0x1020_3040).await;
    }

    #[tokio::test]
    async fn http_ts_pull_republishes_media_frame() {
        let ts = test_ts_stream();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_once(&listener, "video/mp2t", &ts).await;
        });
        let manager = Arc::new(MediaSourceManager::new(None));
        start(
            &format!("http://127.0.0.1:{port}/live/test.ts"),
            "__defaultVhost__",
            "live",
            "http_ts",
            manager.clone(),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let source = manager
            .get("__defaultVhost__", "live", "http_ts")
            .expect("HTTP-TS pull did not create a source");
        assert!(source
            .get_latest_gop_frames()
            .await
            .iter()
            .any(|frame| frame.codec == CodecId::H264));
    }

    #[tokio::test]
    async fn hls_ts_pull_fetches_playlist_and_segment() {
        let ts = test_ts_stream();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let playlist =
            b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nsegment.ts\n#EXT-X-ENDLIST\n"
                .to_vec();
        tokio::spawn(async move {
            serve_once(&listener, "application/vnd.apple.mpegurl", &playlist).await;
            serve_once(&listener, "video/mp2t", &ts).await;
        });
        let manager = Arc::new(MediaSourceManager::new(None));
        start(
            &format!("http://127.0.0.1:{port}/live/index.m3u8"),
            "__defaultVhost__",
            "live",
            "hls_ts",
            manager.clone(),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let source = manager
            .get("__defaultVhost__", "live", "hls_ts")
            .expect("HLS pull did not create a source");
        assert!(source
            .get_latest_gop_frames()
            .await
            .iter()
            .any(|frame| frame.codec == CodecId::H264));
    }

    #[tokio::test]
    async fn hls_fmp4_pull_fetches_init_and_media_segment() {
        let (init, segment) = test_fmp4_stream();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let playlist = b"#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\nsegment.m4s\n#EXT-X-ENDLIST\n".to_vec();
        tokio::spawn(async move {
            serve_once(&listener, "application/vnd.apple.mpegurl", &playlist).await;
            serve_once(&listener, "video/mp4", &init).await;
            serve_once(&listener, "video/iso.segment", &segment).await;
        });
        let manager = Arc::new(MediaSourceManager::new(None));
        start(
            &format!("http://127.0.0.1:{port}/live/index.m3u8"),
            "__defaultVhost__",
            "live",
            "hls_fmp4",
            manager.clone(),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let source = manager
            .get("__defaultVhost__", "live", "hls_fmp4")
            .expect("HLS fMP4 pull did not create a source");
        let frames = source.get_latest_gop_frames().await;
        assert!(frames.iter().any(|frame| {
            frame.codec == CodecId::H264
                && !frame.config_frame
                && frame.payload_format == zlmediakit_core::PayloadFormat::Avcc
        }));
        assert_rtp_relay(manager, "hls_fmp4", 0x5060_7080).await;
    }
}
