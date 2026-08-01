use crate::amf::{AmfDecoder, AmfEncoder, AmfValue};
use crate::handshake::RtmpHandshake;
use crate::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use zlmediakit_core::media_frame::FrameType;
use zlmediakit_core::media_source::MediaSourceManager;

#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
type ConnectStream = Box<dyn IoStream>;

/// Parses an RTMP(S) URL into (host, port, app, stream_name, use_tls).
fn parse_url(url: &str) -> anyhow::Result<(String, u16, String, String, bool)> {
    let (s, use_tls) = if let Some(s) = url.strip_prefix("rtmps://") {
        (s, true)
    } else if let Some(s) = url.strip_prefix("rtmp://") {
        (s, false)
    } else {
        anyhow::bail!("not an RTMP(S) URL: {}", url);
    };
    let default_port = if use_tls { 443 } else { 1935 };
    let (authority, path) = match s.find('/') {
        Some(i) => (s[..i].to_string(), s[i + 1..].to_string()),
        None => (s.to_string(), String::new()),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let p = authority[i + 1..].parse::<u16>().unwrap_or(default_port);
            (authority[..i].to_string(), p)
        }
        None => (authority, default_port),
    };
    let (app, stream) = path
        .split_once('/')
        .map(|(a, s)| (a.to_string(), s.to_string()))
        .unwrap_or_else(|| (path, String::new()));
    Ok((host, port, app, stream, use_tls))
}

async fn connect(url: &str) -> anyhow::Result<(ConnectStream, String, String)> {
    let (host, port, remote_app, remote_stream, use_tls) = parse_url(url)?;
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("connect {}:{} failed: {}", host, port, e))?;
    tcp.set_nodelay(true)?;

    let stream: ConnectStream = if use_tls {
        let config = Arc::new(
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier))
                .with_no_client_auth(),
        );
        let connector = tokio_rustls::TlsConnector::from(config);
        let domain = ServerName::try_from(host.clone())
            .map_err(|_| anyhow::anyhow!("invalid hostname for TLS: {}", host))?;
        Box::new(connector.connect(domain, tcp).await?)
    } else {
        Box::new(tcp)
    };

    Ok((stream, remote_app, remote_stream))
}

/// Push a local stream to a remote RTMP(S) server.
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

    let (mut stream, remote_app, remote_stream) = connect(url).await?;
    let stream_name = if stream_name.is_empty() {
        &remote_stream
    } else {
        stream_name
    };
    let app = if app.is_empty() { "live" } else { app };

    RtmpHandshake::client_handshake(&mut *stream).await?;
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

    let msgs = read_until_result(&mut *stream, &mut parser, &mut read_buf, 1.0).await?;
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

    let msgs = read_until_result(&mut *stream, &mut parser, &mut read_buf, 2.0).await?;
    apply_control_messages(&mut encoder, &msgs);

    let push_stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = AmfDecoder::decode(data).ok()?;
                if vals
                    .first()
                    .is_some_and(|v| matches!(v, AmfValue::String(s) if s == "_result"))
                {
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

    let msgs = read_until_status(&mut *stream, &mut parser, &mut read_buf).await?;
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
                if stopped.load(Ordering::SeqCst) { break; }
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

async fn read_until_result(
    stream: &mut (dyn AsyncRead + Unpin + Send),
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
                    if vals
                        .first()
                        .is_some_and(|v| matches!(v, AmfValue::String(s) if s == "_result"))
                    {
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

async fn read_until_status(
    stream: &mut (dyn AsyncRead + Unpin + Send),
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
                    if vals
                        .first()
                        .is_some_and(|v| matches!(v, AmfValue::String(s) if s == "onStatus"))
                    {
                        all.push(m.clone());
                        return Ok(all);
                    }
                }
            }
            all.push(m.clone());
        }
    }
}

fn apply_control_messages(encoder: &mut RtmpMessageEncoder, msgs: &[RtmpMessage]) {
    for m in msgs {
        if let RtmpMessage::SetChunkSize(size) = m {
            encoder.set_chunk_size(*size as usize);
            debug!("RTMP push: server set chunk size to {}", size);
        }
    }
}
