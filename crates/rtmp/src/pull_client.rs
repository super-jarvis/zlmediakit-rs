use crate::amf::{AmfDecoder, AmfEncoder, AmfValue};
use crate::handshake::RtmpHandshake;
use crate::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use crate::session::{parse_audio_rtmp_packet, parse_video_rtmp_packet};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tracing::{info, warn};
use zlmediakit_core::media_frame::{AudioInfo, MediaFrame, TrackInfo, VideoInfo};
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

/// Pulls a remote RTMP(S) stream and republishes it locally.
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    info!("RTMP pull: {} -> {}/{}/{}", url, vhost, app, stream_name);

    let (mut stream, remote_app, remote_stream) = connect(url).await?;
    let stream_name = if stream_name.is_empty() {
        &remote_stream
    } else {
        stream_name
    };
    let app = if app.is_empty() { "live" } else { app };

    RtmpHandshake::client_handshake(&mut *stream).await?;
    info!("RTMP pull handshake done for {}", url);

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
    info!("RTMP pull connect ok for {}", url);

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
    let pull_stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = AmfDecoder::decode(data).ok()?;
                if vals.first().map_or(false, |v| matches!(v, AmfValue::String(s) if s == "_result")) {
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
        .ok_or_else(|| anyhow::anyhow!("RTMP pull: no stream id from createStream"))?;
    info!("RTMP pull got stream_id={} for {}", pull_stream_id, url);

    // --- play ---
    let play_payload = AmfEncoder::encode(&[
        AmfValue::String("play".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String(remote_stream.clone()),
    ])
    .freeze();
    let play_msg = RtmpMessage::Amf0Command {
        stream_id: pull_stream_id,
        timestamp: 0,
        data: play_payload,
    };
    stream.write_all(&encoder.encode(&play_msg)).await?;
    info!("RTMP pull play sent for {}", url);

    let source = source_manager.get_or_create(vhost, app, stream_name);
    let mut has_video = false;
    let mut has_audio = false;

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let n = tokio::select! {
            _ = stop.notified() => {
                if stopped.load(Ordering::SeqCst) { break; }
                continue;
            }
            n = stream.read(&mut read_buf) => n?,
        };
        if n == 0 {
            info!("RTMP pull: upstream closed {}", url);
            break;
        }
        let msgs = parser.feed(&read_buf[..n]);
        for msg in msgs {
            match msg {
                RtmpMessage::SetChunkSize(sz) => parser.set_chunk_size(sz),
                RtmpMessage::Video { timestamp, data, .. } => {
                    let (codec, key_frame, is_config) = parse_video_rtmp_packet(&data);
                    if is_config && !has_video {
                        has_video = true;
                        source.info.write().await.tracks.push(TrackInfo::Video(VideoInfo {
                            codec, width: 640, height: 480, fps: 25.0, key_frame: false,
                        }));
                    }
                    let mut frame = MediaFrame::new_video(
                        0, codec, timestamp, timestamp as u64, timestamp as u64, data, key_frame,
                    );
                    frame.config_frame = is_config;
                    source.publish_and_cache(frame).await;
                }
                RtmpMessage::Audio { timestamp, data, .. } => {
                    let codec = parse_audio_rtmp_packet(&data);
                    let is_config = data.len() >= 2 && (data[0] >> 4) & 0x0F == 10 && data[1] == 0x00;
                    if is_config && !has_audio {
                        has_audio = true;
                        source.info.write().await.tracks.push(TrackInfo::Audio(AudioInfo {
                            codec, sample_rate: 44100, channels: 2, bits_per_sample: 16,
                        }));
                    }
                    let mut frame = MediaFrame::new_audio(
                        1, codec, timestamp, timestamp as u64, timestamp as u64, data,
                    );
                    frame.config_frame = is_config;
                    source.publish_and_cache(frame).await;
                }
                RtmpMessage::Amf0Command { data, .. } => {
                    if let Ok(vals) = AmfDecoder::decode(&data) {
                        if let Some(AmfValue::String(cmd)) = vals.first() {
                            if cmd == "onStatus" {
                                if let Some(AmfValue::Object(props)) = vals.get(3) {
                                    for (k, v) in props {
                                        if k == "code" {
                                            if let AmfValue::String(s) = v {
                                                if s == "NetStream.Play.StreamNotFound" {
                                                    warn!("RTMP pull: stream not found on remote");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    info!("RTMP pull finished for {}", url);
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
            anyhow::bail!("RTMP pull: connection closed before _result");
        }
        let msgs = parser.feed(&buf[..n]);
        for m in &msgs {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                if let Ok(vals) = AmfDecoder::decode(data) {
                    if vals.first().map_or(false, |v| matches!(v, AmfValue::String(s) if s == "_result")) {
                        let got = vals.get(1).and_then(|v| match v {
                            AmfValue::Number(n) => Some(*n),
                            _ => None,
                        }).unwrap_or(0.0);
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

fn apply_control_messages(encoder: &mut RtmpMessageEncoder, msgs: &[RtmpMessage]) {
    for m in msgs {
        if let RtmpMessage::SetChunkSize(size) = m {
            encoder.set_chunk_size(*size as usize);
        }
    }
}
