//! Minimal HTTP transport for WHEP.
//!
//! WHEP is a tiny HTTP protocol: the client POSTs an SDP offer and gets an SDP
//! answer back (plus a `Location` it later DELETEs to stop). We implement just
//! enough HTTP/1.1 here so the WebRTC crate stays decoupled from the `http`
//! protocol crate (which, per project convention, must not depend on WebRTC).

use anyhow::Result;
use bytes::BytesMut;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};
use uuid::Uuid;
use zlmediakit_core::media_source::MediaSourceManager;

use crate::whep::{whep_play, WhepSession};

pub struct WebRtcServer {
    listener: TcpListener,
    source_manager: Arc<MediaSourceManager>,
    sessions: Arc<DashMap<String, WhepSession>>,
}

impl WebRtcServer {
    pub async fn new(addr: &str, source_manager: Arc<MediaSourceManager>) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        info!("WebRTC (WHEP) server listening on {}", addr);
        Ok(Self {
            listener,
            source_manager,
            sessions: Arc::new(DashMap::new()),
        })
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, _peer) = self.listener.accept().await?;
            let sm = self.source_manager.clone();
            let sessions = self.sessions.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, sm, sessions).await {
                    error!("webrtc: conn error: {}", e);
                }
            });
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    sm: Arc<MediaSourceManager>,
    sessions: Arc<DashMap<String, WhepSession>>,
) -> Result<()> {
    let mut buf = BytesMut::new();
    let mut tmp = [0u8; 8192];

    // Read until we have the full headers and body (signalled by Content-Length).
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);

        let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
        if let Some(h) = header_end {
            let body_len = content_length(&buf);
            if buf.len() >= h + body_len {
                break;
            }
        }
        if buf.len() > 1 << 20 {
            return Ok(()); // safety: ignore absurd requests
        }
    }

    let (method, path, query, body) = parse_request(&buf)?;
    match method.as_str() {
        "POST" => handle_post(&mut stream, &path, &query, &body, &sm, &sessions).await,
        "DELETE" => handle_delete(&mut stream, &path, &sessions).await,
        _ => write_response(&mut stream, 405, "Method Not Allowed", &[], &[]).await,
    }
}

fn resolve_stream(segs: &[&str], query: &HashMap<String, String>) -> (String, String, String) {
    if segs.len() >= 3 {
        (
            segs[0].to_string(),
            segs[1].to_string(),
            segs[2].to_string(),
        )
    } else {
        (
            query
                .get("vhost")
                .cloned()
                .unwrap_or_else(|| "__defaultVhost__".to_string()),
            query
                .get("app")
                .cloned()
                .unwrap_or_else(|| "live".to_string()),
            query.get("stream").cloned().unwrap_or_default(),
        )
    }
}

async fn handle_post(
    stream: &mut TcpStream,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    sm: &Arc<MediaSourceManager>,
    sessions: &Arc<DashMap<String, WhepSession>>,
) -> Result<()> {
    let segs: Vec<&str> = path
        .trim_start_matches("/webrtc/play/")
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let (vhost, app, stream_id) = resolve_stream(&segs, query);

    if stream_id.is_empty() {
        return write_response(
            stream,
            400,
            "Bad Request",
            &[],
            b"missing stream identifier",
        )
        .await;
    }

    let source = match sm.get(&vhost, &app, &stream_id) {
        Some(s) => s,
        None => {
            return write_response(stream, 404, "Not Found", &[], b"stream not found").await;
        }
    };

    let offer = String::from_utf8_lossy(body).to_string();
    let resource = Uuid::new_v4().to_string();
    match whep_play(&offer, source, resource.clone()).await {
        Ok((answer, session)) => {
            sessions.insert(resource.clone(), session);
            let location = format!("/webrtc/play/{}", resource);
            write_response(
                stream,
                201,
                "Created",
                &[
                    ("Content-Type", "application/sdp"),
                    ("Location", &location),
                    ("ETag", &resource),
                    ("Access-Control-Allow-Origin", "*"),
                ],
                answer.as_bytes(),
            )
            .await
        }
        Err(e) => {
            error!(
                "webrtc: whep_play failed for {}/{}/{}: {}",
                vhost, app, stream_id, e
            );
            write_response(
                stream,
                500,
                "Internal Server Error",
                &[],
                b"negotiation failed",
            )
            .await
        }
    }
}

async fn handle_delete(
    stream: &mut TcpStream,
    path: &str,
    sessions: &Arc<DashMap<String, WhepSession>>,
) -> Result<()> {
    let resource = path
        .trim_start_matches("/webrtc/play/")
        .trim_end_matches('/')
        .to_string();
    if let Some((_, session)) = sessions.remove(&resource) {
        session.close().await;
        write_response(stream, 200, "OK", &[], b"stopped").await
    } else {
        write_response(stream, 404, "Not Found", &[], b"unknown resource").await
    }
}

fn content_length(buf: &BytesMut) -> usize {
    let head = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => &buf[..p],
        None => return 0,
    };
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if let Some(val) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            if let Ok(n) = val.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

#[allow(clippy::type_complexity)]
fn parse_request(buf: &BytesMut) -> Result<(String, String, HashMap<String, String>, Vec<u8>)> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, HashMap::new()),
    };

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let body = buf[header_end..].to_vec();

    Ok((method, path, query, body))
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.to_string(), v.replace('+', " "));
        }
    }
    map
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let mut resp = format!("HTTP/1.1 {} {}\r\n", status, reason);
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    for (k, v) in headers {
        resp.push_str(&format!("{}: {}\r\n", k, v));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    Ok(())
}
