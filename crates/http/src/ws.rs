use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::{Buf, BytesMut};
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{FrameType, TrackInfo};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::transport::TransportStream;
use zlmediakit_flv::FlvMuxer;
use zlmediakit_hls::TsLiveMuxer;
use zlmediakit_mp4::fmp4::Fmp4Muxer;

const MAGIC_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub fn compute_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(MAGIC_GUID.as_bytes());
    BASE64.encode(hasher.finalize())
}

pub fn upgrade_response(key: &str) -> String {
    let accept = compute_accept_key(key);
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        accept
    )
}

#[derive(Debug)]
pub struct WsFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl WsFrame {
    pub fn binary(data: Vec<u8>) -> Self {
        Self {
            fin: true,
            opcode: 0x2,
            payload: data,
        }
    }

    pub fn pong() -> Self {
        Self {
            fin: true,
            opcode: 0xA,
            payload: vec![],
        }
    }

    pub fn close(code: u16, reason: &str) -> Self {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        Self {
            fin: true,
            opcode: 0x8,
            payload,
        }
    }

    pub fn encode_server(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push((self.fin as u8) << 7 | self.opcode);
        let len = self.payload.len();
        if len < 126 {
            buf.push(len as u8);
        } else if len <= 0xFFFF {
            buf.push(126);
            buf.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            buf.push(127);
            buf.extend_from_slice(&(len as u64).to_be_bytes());
        }
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < 2 {
            return None;
        }
        let b0 = buf[0];
        let b1 = buf[1];
        let fin = (b0 & 0x80) != 0;
        let opcode = b0 & 0x0F;
        let masked = (b1 & 0x80) != 0;
        let mut len = (b1 & 0x7F) as usize;
        let mut offset = 2usize;

        if len == 126 {
            if buf.len() < 4 {
                return None;
            }
            len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            offset = 4;
        } else if len == 127 {
            if buf.len() < 10 {
                return None;
            }
            len = u64::from_be_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]) as usize;
            offset = 10;
        }

        let mask_key: [u8; 4];
        if masked {
            if buf.len() < offset + 4 {
                return None;
            }
            mask_key = [
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ];
            offset += 4;
        } else {
            mask_key = [0; 4];
        }

        if buf.len() < offset + len {
            return None;
        }

        let mut payload = buf[offset..offset + len].to_vec();
        if masked {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask_key[i % 4];
            }
        }

        Some((
            Self {
                fin,
                opcode,
                payload,
            },
            offset + len,
        ))
    }
}

pub struct WsSession {
    stream: TransportStream,
    read_buf: BytesMut,
    source_manager: Arc<MediaSourceManager>,
    auth: Arc<StreamAuth>,
    hook: Arc<HookClient>,
}

impl WsSession {
    pub fn new(
        stream: TransportStream,
        source_manager: Arc<MediaSourceManager>,
        auth: Arc<StreamAuth>,
        hook: Arc<HookClient>,
    ) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(8192),
            source_manager,
            auth,
            hook,
        }
    }

    pub async fn run_flv(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".flv")
            .map_or("stream", |v| v)
            .to_string();

        // External hook callback (before built-in auth)
        if let zlmediakit_core::hook::HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "WS-FLV play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("WS-FLV play rejected (auth): {}/{}", app, stream_name);
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        let source = self
            .source_manager
            .get("__defaultVhost__", &app, &stream_name);

        let source = match source {
            Some(s) => s,
            None => {
                let close = WsFrame::close(3004, "stream not found");
                self.stream.write_all(&close.encode_server()).await?;
                return Ok(());
            }
        };

        let mut muxer = FlvMuxer::new();
        let header = muxer.write_header();
        self.stream
            .write_all(&WsFrame::binary(header.to_vec()).encode_server())
            .await?;

        let info = source.info.read().await;
        if let Some(TrackInfo::Video(ref v)) = info.tracks.first() {
            let meta = muxer.write_metadata(v.width, v.height, v.fps, 44100, v.codec);
            self.stream
                .write_all(&WsFrame::binary(meta.to_vec()).encode_server())
                .await?;
        }
        drop(info);

        let cached = {
            let cache = source.gop_cache.read().await;
            cache.get_latest_gop_frames()
        };

        for frame in &cached {
            let tag = muxer.write_tag(frame)?;
            self.stream
                .write_all(&WsFrame::binary(tag.to_vec()).encode_server())
                .await?;
        }

        let mut rx = source.subscribe();
        let mut rx_closed = false;

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                                continue;
                            }
                            let tag = muxer.write_tag(&frame)?;
                            let ws_frame = WsFrame::binary(tag.to_vec());
                            if let Err(e) = self.stream.write_all(&ws_frame.encode_server()).await {
                                debug!("WS write error: {}", e);
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            rx_closed = true;
                            break;
                        }
                    }
                }
                read_result = self.read_frame() => {
                    match read_result {
                        Some(frame) => {
                            match frame.opcode {
                                0x8 => {
                                    debug!("WS close frame received");
                                    let close = WsFrame::close(1000, "bye");
                                    let _ = self.stream.write_all(&close.encode_server()).await;
                                    break;
                                }
                                0x9 => {
                                    let pong = WsFrame::pong();
                                    if let Err(e) = self.stream.write_all(&pong.encode_server()).await {
                                        debug!("WS pong write error: {}", e);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        if rx_closed {
            let close = WsFrame::close(1001, "source gone");
            let _ = self.stream.write_all(&close.encode_server()).await;
        }

        Ok(())
    }

    /// WebSocket MPEG-TS playback (`/app/stream.live.ts` over WebSocket).
    pub async fn run_ts(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".live.ts")
            .map_or("stream", |v| v)
            .to_string();

        if let zlmediakit_core::hook::HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "WS-TS play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("WS-TS play rejected (auth): {}/{}", app, stream_name);
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        let source = match self
            .source_manager
            .get("__defaultVhost__", &app, &stream_name)
        {
            Some(s) => s,
            None => {
                let close = WsFrame::close(3004, "stream not found");
                self.stream.write_all(&close.encode_server()).await?;
                return Ok(());
            }
        };

        let mut muxer = TsLiveMuxer::new();

        let configs = {
            let cache = source.gop_cache.read().await;
            cache.get_config_frames()
        };
        for frame in &configs {
            let ts = muxer.push_frame(frame);
            if !ts.is_empty() {
                self.stream
                    .write_all(&WsFrame::binary(ts.to_vec()).encode_server())
                    .await?;
            }
        }

        let cached = {
            let cache = source.gop_cache.read().await;
            cache.get_latest_gop_frames()
        };
        for frame in &cached {
            let ts = muxer.push_frame(frame);
            if !ts.is_empty() {
                self.stream
                    .write_all(&WsFrame::binary(ts.to_vec()).encode_server())
                    .await?;
            }
        }

        let mut rx = source.subscribe();
        let mut rx_closed = false;
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            let ts = muxer.push_frame(&frame);
                            if !ts.is_empty() {
                                let ws_frame = WsFrame::binary(ts.to_vec());
                                if let Err(e) = self.stream.write_all(&ws_frame.encode_server()).await {
                                    debug!("WS-TS write error: {}", e);
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            rx_closed = true;
                            break;
                        }
                    }
                }
                read_result = self.read_frame() => {
                    match read_result {
                        Some(frame) => {
                            match frame.opcode {
                                0x8 => {
                                    debug!("WS close frame received");
                                    let close = WsFrame::close(1000, "bye");
                                    let _ = self.stream.write_all(&close.encode_server()).await;
                                    break;
                                }
                                0x9 => {
                                    let pong = WsFrame::pong();
                                    if let Err(e) = self.stream.write_all(&pong.encode_server()).await {
                                        debug!("WS pong write error: {}", e);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        if rx_closed {
            let close = WsFrame::close(1001, "source gone");
            let _ = self.stream.write_all(&close.encode_server()).await;
        }

        Ok(())
    }

    /// WebSocket live fMP4 playback (`/app/stream.live.mp4` over WebSocket).
    pub async fn run_fmp4(&mut self, path: &str) -> anyhow::Result<()> {
        let (app, resource, sign) = parse_path_sign(path);
        let stream_name = resource
            .strip_suffix(".live.mp4")
            .map_or("stream", |v| v)
            .to_string();

        if let zlmediakit_core::hook::HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &app, &stream_name, &sign)
            .await
        {
            warn!(
                "WS-fMP4 play rejected (hook): {}/{} - {}",
                app, stream_name, msg
            );
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        if !self
            .auth
            .check("__defaultVhost__", &app, &stream_name, "play", &sign)
        {
            warn!("WS-fMP4 play rejected (auth): {}/{}", app, stream_name);
            let close = WsFrame::close(3000, "unauthorized");
            self.stream.write_all(&close.encode_server()).await?;
            return Ok(());
        }

        let source = match self
            .source_manager
            .get("__defaultVhost__", &app, &stream_name)
        {
            Some(s) => s,
            None => {
                let close = WsFrame::close(3004, "stream not found");
                self.stream.write_all(&close.encode_server()).await?;
                return Ok(());
            }
        };

        let mut muxer = Fmp4Muxer::new(90_000);

        let configs = {
            let cache = source.gop_cache.read().await;
            cache.get_config_frames()
        };
        for frame in &configs {
            muxer.push_frame(frame);
        }
        let init = muxer.init_segment();
        self.stream
            .write_all(&WsFrame::binary(init.data.to_vec()).encode_server())
            .await?;

        let cached = {
            let cache = source.gop_cache.read().await;
            cache.get_latest_gop_frames()
        };
        for frame in &cached {
            if zlmediakit_core::gop_cache::is_config_frame(frame) {
                continue;
            }
            muxer.push_frame(frame);
            if frame.frame_type == FrameType::Video && frame.key_frame {
                if let Some(seg) = muxer.flush_segment() {
                    let ws_frame = WsFrame::binary(seg.data.to_vec());
                    self.stream.write_all(&ws_frame.encode_server()).await?;
                }
            }
        }
        if let Some(seg) = muxer.flush_segment() {
            let ws_frame = WsFrame::binary(seg.data.to_vec());
            self.stream.write_all(&ws_frame.encode_server()).await?;
        }

        let mut rx = source.subscribe();
        let mut rx_closed = false;
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                                continue;
                            }
                            muxer.push_frame(&frame);
                            if frame.frame_type == FrameType::Video && frame.key_frame {
                                if let Some(seg) = muxer.flush_segment() {
                                    let ws_frame = WsFrame::binary(seg.data.to_vec());
                                    if let Err(e) = self.stream.write_all(&ws_frame.encode_server()).await {
                                        debug!("WS-fMP4 write error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            rx_closed = true;
                            break;
                        }
                    }
                }
                read_result = self.read_frame() => {
                    match read_result {
                        Some(frame) => {
                            match frame.opcode {
                                0x8 => {
                                    debug!("WS close frame received");
                                    let close = WsFrame::close(1000, "bye");
                                    let _ = self.stream.write_all(&close.encode_server()).await;
                                    break;
                                }
                                0x9 => {
                                    let pong = WsFrame::pong();
                                    if let Err(e) = self.stream.write_all(&pong.encode_server()).await {
                                        debug!("WS pong write error: {}", e);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        if rx_closed {
            let close = WsFrame::close(1001, "source gone");
            let _ = self.stream.write_all(&close.encode_server()).await;
        }

        Ok(())
    }

    async fn read_frame(&mut self) -> Option<WsFrame> {
        loop {
            if let Some((frame, consumed)) = WsFrame::decode(&self.read_buf[..]) {
                self.read_buf.advance(consumed);
                return Some(frame);
            }

            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf).await {
                Ok(0) => return None,
                Ok(n) => {
                    self.read_buf.extend_from_slice(&buf[..n]);
                }
                Err(e) => {
                    debug!("WS read error: {}", e);
                    return None;
                }
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_known_input() {
        // RFC 6455 example: key="dGhlIHNhbXBsZSBub25jZQ=="
        // accept should be "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = compute_accept_key(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn upgrade_response_contains_101() {
        let resp = upgrade_response("test-key");
        assert!(resp.starts_with("HTTP/1.1 101"));
        assert!(resp.contains("Upgrade: websocket"));
        assert!(resp.contains("Sec-WebSocket-Accept:"));
    }

    #[test]
    fn encode_binary_frame() {
        let frame = WsFrame::binary(vec![0x01, 0x02, 0x03]);
        let encoded = frame.encode_server();
        // fin=1, opcode=0x2 => 0x82, len=3 => 0x03
        assert_eq!(encoded, vec![0x82, 0x03, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn encode_binary_frame_large() {
        let payload = vec![0xABu8; 200];
        let frame = WsFrame::binary(payload.clone());
        let encoded = frame.encode_server();
        assert_eq!(encoded[0], 0x82);
        assert_eq!(encoded[1], 126);
        assert_eq!(u16::from_be_bytes([encoded[2], encoded[3]]), 200);
        assert_eq!(&encoded[4..], &payload[..]);
    }

    #[test]
    fn encode_pong_frame() {
        let frame = WsFrame::pong();
        let encoded = frame.encode_server();
        assert_eq!(encoded, vec![0x8A, 0x00]);
    }

    #[test]
    fn encode_close_frame() {
        let frame = WsFrame::close(1000, "bye");
        let encoded = frame.encode_server();
        // fin=1 opcode=0x8 => 0x88, len=5 => 0x05, payload=0x03E8 + "bye"
        assert_eq!(encoded[0], 0x88);
        assert_eq!(encoded[1], 5);
        assert_eq!(&encoded[2..4], &1000u16.to_be_bytes());
        assert_eq!(&encoded[4..], b"bye");
    }

    #[test]
    fn decode_unmasked_frame() {
        let data = vec![0x82, 0x03, 0x01, 0x02, 0x03];
        let (frame, consumed) = WsFrame::decode(&data).unwrap();
        assert_eq!(consumed, 5);
        assert!(frame.fin);
        assert_eq!(frame.opcode, 0x2);
        assert_eq!(frame.payload, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn decode_masked_frame() {
        let mut data = vec![0x82, 0x83]; // fin=1, opcode=2, mask=1, len=3
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        data.extend_from_slice(&mask);
        // payload = [0x01, 0x02, 0x03] XOR mask
        let masked: Vec<u8> = [0x01, 0x02, 0x03]
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ mask[i % 4])
            .collect();
        data.extend_from_slice(&masked);
        let (frame, consumed) = WsFrame::decode(&data).unwrap();
        assert_eq!(consumed, 9);
        assert_eq!(frame.payload, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn decode_short_buffer_returns_none() {
        assert!(WsFrame::decode(b"\x82").is_none());
    }

    #[test]
    fn decode_incomplete_payload_returns_none() {
        // claims len=10 but only 5 bytes provided
        let data = vec![0x82, 10, 1, 2, 3, 4, 5];
        assert!(WsFrame::decode(&data).is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"Hello, World!".to_vec();
        let frame = WsFrame::binary(payload.clone());
        let encoded = frame.encode_server();
        let (decoded, _) = WsFrame::decode(&encoded).unwrap();
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn encode_decode_large_roundtrip() {
        let payload = vec![0x42u8; 50000];
        let frame = WsFrame::binary(payload.clone());
        let encoded = frame.encode_server();
        let (decoded, _) = WsFrame::decode(&encoded).unwrap();
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn parse_path_sign_basic() {
        let (app, resource, sign) = parse_path_sign("/live/stream.flv");
        assert_eq!(app, "live");
        assert_eq!(resource, "stream.flv");
        assert_eq!(sign, "");
    }

    #[test]
    fn parse_path_sign_with_sign() {
        let (app, resource, sign) = parse_path_sign("/live/stream.flv?sign=abc123");
        assert_eq!(app, "live");
        assert_eq!(resource, "stream.flv");
        assert_eq!(sign, "abc123");
    }

    #[test]
    fn parse_path_sign_no_leading_slash() {
        let (app, resource, _) = parse_path_sign("app/stream.flv");
        assert_eq!(app, "app");
        assert_eq!(resource, "stream.flv");
    }

    #[test]
    fn parse_path_sign_sign_with_other_params() {
        let (_, _, sign) = parse_path_sign("/a/b?foo=1&sign=tok&bar=2");
        assert_eq!(sign, "tok");
    }
}
