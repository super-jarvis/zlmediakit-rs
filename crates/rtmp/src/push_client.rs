use crate::amf::{AmfDecoder, AmfEncoder, AmfValue};
use crate::handshake::RtmpHandshake;
use crate::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use zlmediakit_core::media_frame::FrameType;
use zlmediakit_core::media_source::MediaSourceManager;

/// Parses an RTMP URL into (host, port, app, stream_name).
/// Supports `rtmp://host:port/app/stream` and `rtmp://host/app/stream`.
fn parse_rtmp_url(url: &str) -> anyhow::Result<(String, u16, String, String)> {
    let s = url
        .strip_prefix("rtmp://")
        .ok_or_else(|| anyhow::anyhow!("not an RTMP URL: {}", url))?;
    let (authority, path) = match s.find('/') {
        Some(i) => (s[..i].to_string(), s[i + 1..].to_string()),
        None => (s.to_string(), String::new()),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let p = authority[i + 1..].parse::<u16>().unwrap_or(1935);
            (authority[..i].to_string(), p)
        }
        None => (authority, 1935),
    };
    let (app, stream) = path
        .split_once('/')
        .map(|(a, s)| (a.to_string(), s.to_string()))
        .unwrap_or_else(|| (path, String::new()));
    Ok((host, port, app, stream))
}

/// Push a local stream to a remote RTMP server.
///
/// Connects to the given RTMP URL, performs handshake and RTMP
/// publish handshake, then subscribes to the local MediaSource and
/// forwards all frames to the remote server until `stop` is signalled
/// or the upstream disconnects.
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    info!("RTMP push: {} -> {}/{}/{}", url, vhost, app, stream_name);

    let (remote_host, remote_port, remote_app, remote_stream) = parse_rtmp_url(url)?;
    let stream_name = if stream_name.is_empty() {
        &remote_stream
    } else {
        stream_name
    };
    let app = if app.is_empty() { "live" } else { app };

    let mut stream = TcpStream::connect((remote_host.as_str(), remote_port))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "RTMP push connect to {}:{} failed: {}",
                remote_host,
                remote_port,
                e
            )
        })?;
    stream.set_nodelay(true).ok();

    RtmpHandshake::client_handshake(&mut stream).await?;
    info!("RTMP push handshake done for {}", url);

    let mut parser = RtmpMessageParser::new();
    let mut encoder = RtmpMessageEncoder::new();
    let mut read_buf = [0u8; 65536];

    // --- connect ---
    let connect_payload = AmfEncoder::encode(&[
        AmfValue::String("connect".to_string()),
        AmfValue::Number(1.0),
        AmfValue::Object(vec![
            ("app".to_string(), AmfValue::String(remote_app.clone())),
            ("tcUrl".to_string(), AmfValue::String(url.to_string())),
        ]),
    ])
    .freeze();
    let connect_msg = RtmpMessage::Amf0Command {
        stream_id: 0,
        timestamp: 0,
        data: connect_payload,
    };
    stream.write_all(&encoder.encode(&connect_msg)).await?;

    // Read until we get _result for connect (transaction id 1)
    let msgs = read_until_result(&mut stream, &mut parser, &mut read_buf, 1.0).await?;
    apply_control_messages(&mut encoder, &msgs);

    info!("RTMP push connect ok for {}", url);

    // --- createStream ---
    let cs_payload = AmfEncoder::encode(&[
        AmfValue::String("createStream".to_string()),
        AmfValue::Number(2.0),
        AmfValue::Null,
    ])
    .freeze();
    let cs_msg = RtmpMessage::Amf0Command {
        stream_id: 0,
        timestamp: 0,
        data: cs_payload,
    };
    stream.write_all(&encoder.encode(&cs_msg)).await?;

    let msgs = read_until_result(&mut stream, &mut parser, &mut read_buf, 2.0).await?;
    apply_control_messages(&mut encoder, &msgs);

    let push_stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = AmfDecoder::decode(data).ok()?;
                if vals.first().map_or(
                    false,
                    |v| matches!(v, AmfValue::String(s) if s == "_result"),
                ) {
                    vals.get(3).and_then(|v| match v {
                        AmfValue::Number(n) => Some(*n as u32),
                        _ => None,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("RTMP push: no stream id from createStream"))?;

    info!("RTMP push got stream_id={} for {}", push_stream_id, url);

    // --- publish ---
    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String(remote_stream.clone()),
        AmfValue::String("live".to_string()),
    ])
    .freeze();
    let pub_msg = RtmpMessage::Amf0Command {
        stream_id: push_stream_id,
        timestamp: 0,
        data: pub_payload,
    };
    stream.write_all(&encoder.encode(&pub_msg)).await?;

    // Read until we get onStatus for publish
    let msgs = read_until_status(&mut stream, &mut parser, &mut read_buf).await?;
    apply_control_messages(&mut encoder, &msgs);

    info!("RTMP push publish ok for {}", url);

    // --- Subscribe to local source and forward frames ---
    let source = source_manager.get(vhost, app, stream_name).ok_or_else(|| {
        anyhow::anyhow!(
            "RTMP push: local stream {}/{}/{} not found",
            vhost,
            app,
            stream_name
        )
    })?;

    let mut rx = source.subscribe();

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = stop.notified() => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
            }
            frame = rx.recv() => {
                match frame {
                    Ok(frame) => {
                        let msg = match frame.frame_type {
                            FrameType::Video => RtmpMessage::Video {
                                stream_id: push_stream_id,
                                timestamp: frame.timestamp,
                                data: frame.data,
                            },
                            FrameType::Audio => RtmpMessage::Audio {
                                stream_id: push_stream_id,
                                timestamp: frame.timestamp,
                                data: frame.data,
                            },
                            FrameType::Metadata => RtmpMessage::Amf0Data {
                                stream_id: push_stream_id,
                                timestamp: frame.timestamp,
                                data: frame.data,
                            },
                        };
                        if let Err(e) = stream.write_all(&encoder.encode(&msg)).await {
                            warn!("RTMP push write error {}: {}", url, e);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    info!("RTMP push finished for {}", url);
    Ok(())
}

/// Read incoming RTMP data until we find an AMF `_result` command with
/// the given transaction id. Returns all messages read so far.
async fn read_until_result(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    buf: &mut [u8],
    txn_id: f64,
) -> anyhow::Result<Vec<RtmpMessage>> {
    let mut all = Vec::new();
    loop {
        let n = stream.read(buf).await?;
        if n == 0 {
            anyhow::bail!("RTMP push: connection closed before _result");
        }
        let msgs = parser.feed(&buf[..n]);
        for m in &msgs {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                if let Ok(vals) = AmfDecoder::decode(data) {
                    if vals.first().map_or(
                        false,
                        |v| matches!(v, AmfValue::String(s) if s == "_result"),
                    ) {
                        let got = vals
                            .get(1)
                            .and_then(|v| match v {
                                AmfValue::Number(n) => Some(*n),
                                _ => None,
                            })
                            .unwrap_or(0.0);
                        if (got - txn_id).abs() < 0.001 {
                            all.push(m.clone());
                            return Ok(all);
                        }
                    }
                }
            }
            all.push(m.clone());
        }
    }
}

/// Read incoming RTMP data until we find an AMF `onStatus` command
/// (indicating the publish has started).
async fn read_until_status(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    buf: &mut [u8],
) -> anyhow::Result<Vec<RtmpMessage>> {
    let mut all = Vec::new();
    loop {
        let n = stream.read(buf).await?;
        if n == 0 {
            anyhow::bail!("RTMP push: connection closed before onStatus");
        }
        let msgs = parser.feed(&buf[..n]);
        for m in &msgs {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                if let Ok(vals) = AmfDecoder::decode(data) {
                    if vals.first().map_or(
                        false,
                        |v| matches!(v, AmfValue::String(s) if s == "onStatus"),
                    ) {
                        all.push(m.clone());
                        return Ok(all);
                    }
                }
            }
            all.push(m.clone());
        }
    }
}

/// Apply control messages (SetChunkSize, SetPeerBandwidth, etc.) from the
/// server to our encoder and parser.
fn apply_control_messages(encoder: &mut RtmpMessageEncoder, msgs: &[RtmpMessage]) {
    for m in msgs {
        match m {
            RtmpMessage::SetChunkSize(size) => {
                encoder.set_chunk_size(*size as usize);
                debug!("RTMP push: server set chunk size to {}", size);
            }
            _ => {}
        }
    }
}
