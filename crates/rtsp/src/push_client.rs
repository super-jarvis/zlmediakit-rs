//! RTSP RECORD client for actively pushing a local MediaSource upstream.

use crate::client_transport::{connect, ClientStream};
use crate::parser::{RtspParser, RtspResponse};
use anyhow::{anyhow, bail, Context, Result};
use bytes::{Buf, BytesMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tracing::info;
use zlmediakit_core::auth::md5_hex;
use zlmediakit_core::media_frame::{CodecId, FrameType, TrackInfo};
use zlmediakit_core::payload::aac_audio_specific_config;
use zlmediakit_core::rtp::RtpPacketizer;
use zlmediakit_core::MediaSourceManager;

struct PushTrack {
    frame_type: FrameType,
    channel: u8,
    packetizer: RtpPacketizer,
}

struct PushUrl {
    host: String,
    port: u16,
    uri: String,
    username: Option<String>,
    password: Option<String>,
    secure: bool,
}

fn parse_url(url: &str) -> Result<PushUrl> {
    let (rest, scheme, secure, default_port) = if let Some(rest) = url.strip_prefix("rtsps://") {
        (rest, "rtsps", true, 322)
    } else if let Some(rest) = url.strip_prefix("rtsp://") {
        (rest, "rtsp", false, 554)
    } else {
        bail!("RTSP push requires an rtsp:// or rtsps:// URL");
    };
    let (authority_with_auth, path) = rest
        .find('/')
        .map_or((rest, "/"), |index| (&rest[..index], &rest[index..]));
    let (credentials, authority) = authority_with_auth
        .rsplit_once('@')
        .map_or((None, authority_with_auth), |(credentials, authority)| {
            (Some(credentials), authority)
        });
    let (username, password) = credentials.map_or((None, None), |credentials| {
        let (username, password) = credentials.split_once(':').unwrap_or((credentials, ""));
        (Some(username.to_string()), Some(password.to_string()))
    });
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed IPv6 RTSP host"))?;
        let suffix = &bracketed[end + 1..];
        (
            bracketed[..end].to_string(),
            suffix
                .strip_prefix(':')
                .map(str::parse)
                .transpose()?
                .unwrap_or(default_port),
        )
    } else if authority.matches(':').count() == 1 {
        let (host, port) = authority.rsplit_once(':').expect("count checked");
        (host.to_string(), port.parse()?)
    } else {
        (authority.to_string(), default_port)
    };
    let uri_authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Ok(PushUrl {
        host,
        port,
        uri: format!("{scheme}://{uri_authority}{path}"),
        username,
        password,
        secure,
    })
}

async fn send_request(
    stream: &mut ClientStream,
    method: &str,
    uri: &str,
    cseq: &mut u32,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<()> {
    let mut request = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", *cseq);
    *cseq += 1;
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    Ok(())
}

async fn read_response(stream: &mut ClientStream, buffer: &mut BytesMut) -> Result<RtspResponse> {
    let mut temp = [0u8; 4096];
    loop {
        if let Some((response, consumed)) = RtspParser::parse_response(buffer) {
            buffer.advance(consumed);
            return Ok(response);
        }
        let count = stream.read(&mut temp).await?;
        if count == 0 {
            bail!("RTSP upstream closed during negotiation");
        }
        buffer.extend_from_slice(&temp[..count]);
    }
}

fn ensure_success(response: RtspResponse) -> Result<RtspResponse> {
    if response.status_code / 100 != 2 {
        bail!(
            "RTSP upstream returned {} {}",
            response.status_code,
            response.reason
        );
    }
    Ok(response)
}

fn header<'a>(response: &'a RtspResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn digest_authorization(
    challenge: &str,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> Result<String> {
    let parameters = challenge
        .trim()
        .strip_prefix("Digest ")
        .unwrap_or(challenge);
    let mut realm = None;
    let mut nonce = None;
    for field in parameters.split(',') {
        if let Some((name, value)) = field.trim().split_once('=') {
            let value = value.trim().trim_matches('"');
            match name.trim().to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value.to_string()),
                "nonce" => nonce = Some(value.to_string()),
                _ => {}
            }
        }
    }
    let realm = realm.ok_or_else(|| anyhow!("RTSP Digest challenge is missing realm"))?;
    let nonce = nonce.ok_or_else(|| anyhow!("RTSP Digest challenge is missing nonce"))?;
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
    Ok(format!(
        "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\""
    ))
}

fn make_sdp(tracks: &[TrackInfo], audio_config: Option<&[u8]>) -> Result<String> {
    let mut sdp =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=zlmediakit-rs\r\nt=0 0\r\na=sendonly\r\n".to_string();
    for (index, track) in tracks.iter().enumerate() {
        match track {
            TrackInfo::Video(video) if matches!(video.codec, CodecId::H264 | CodecId::H265) => {
                let codec = if video.codec == CodecId::H265 {
                    "H265"
                } else {
                    "H264"
                };
                sdp.push_str(&format!(
                    "m=video 0 RTP/AVP 96\r\na=rtpmap:96 {codec}/90000\r\na=control:trackID={index}\r\n"
                ));
            }
            TrackInfo::Audio(audio) if audio.codec == CodecId::AAC => {
                let config = audio_config.ok_or_else(|| anyhow!("AAC config frame is required"))?;
                let config_hex = config
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                sdp.push_str(&format!(
                    "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/{}/{}\r\na=fmtp:97 streamtype=5; profile-level-id=1; mode=AAC-hbr; config={config_hex}; SizeLength=13; IndexLength=3; IndexDeltaLength=3\r\na=control:trackID={index}\r\n",
                    audio.sample_rate.max(1),
                    audio.channels.max(1)
                ));
            }
            _ => bail!("RTSP push does not support track codec {:?}", track),
        }
    }
    Ok(sdp)
}

/// Push a local H.264/H.265 + optional AAC source using RTSP RECORD and
/// RFC2326 TCP-interleaved RTP.
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
    let source = source_manager
        .get(vhost, app, stream_name)
        .ok_or_else(|| anyhow!("media source not found"))?;
    let source_tracks = source.info.read().await.tracks.clone();
    let video = source_tracks
        .iter()
        .find(|track| matches!(track, TrackInfo::Video(video) if matches!(video.codec, CodecId::H264 | CodecId::H265)))
        .cloned()
        .ok_or_else(|| anyhow!("RTSP push requires an H.264 or H.265 video track"))?;
    let mut tracks = vec![video];
    if let Some(audio) = source_tracks
        .iter()
        .find(|track| matches!(track, TrackInfo::Audio(audio) if audio.codec == CodecId::AAC))
    {
        tracks.push(audio.clone());
    }
    let cached = source.get_latest_gop_frames().await;
    let audio_config = cached
        .iter()
        .find(|frame| frame.codec == CodecId::AAC && frame.config_frame)
        .map(aac_audio_specific_config)
        .transpose()?;
    let sdp = make_sdp(&tracks, audio_config.as_deref())?;
    let target = parse_url(url)?;
    let mut socket = connect(&target.host, target.port, target.secure).await?;
    let mut buffer = BytesMut::new();
    let mut cseq = 1;

    send_request(
        &mut socket,
        "ANNOUNCE",
        &target.uri,
        &mut cseq,
        &[("Content-Type", "application/sdp".to_string())],
        sdp.as_bytes(),
    )
    .await?;
    let mut announce = read_response(&mut socket, &mut buffer).await?;
    if announce.status_code == 401 {
        let challenge = header(&announce, "WWW-Authenticate")
            .ok_or_else(|| anyhow!("RTSP 401 response is missing WWW-Authenticate"))?;
        let username = target
            .username
            .as_deref()
            .ok_or_else(|| anyhow!("RTSP upstream requires credentials"))?;
        let password = target.password.as_deref().unwrap_or_default();
        let authorization =
            digest_authorization(challenge, username, password, "ANNOUNCE", &target.uri)?;
        send_request(
            &mut socket,
            "ANNOUNCE",
            &target.uri,
            &mut cseq,
            &[
                ("Content-Type", "application/sdp".to_string()),
                ("Authorization", authorization),
            ],
            sdp.as_bytes(),
        )
        .await?;
        announce = read_response(&mut socket, &mut buffer).await?;
    }
    let announce = ensure_success(announce)?;
    let mut session = header(&announce, "Session")
        .map(str::to_string)
        .unwrap_or_default();
    let mut push_tracks = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let channel = (index * 2) as u8;
        let setup_url = format!("{}/trackID={index}", target.uri.trim_end_matches('/'));
        let mut headers = vec![(
            "Transport",
            format!("RTP/AVP/TCP;unicast;interleaved={channel}-{}", channel + 1),
        )];
        if !session.is_empty() {
            headers.push(("Session", session.clone()));
        }
        send_request(&mut socket, "SETUP", &setup_url, &mut cseq, &headers, &[]).await?;
        let response = ensure_success(read_response(&mut socket, &mut buffer).await?)?;
        if session.is_empty() {
            session = header(&response, "Session")
                .map(|value| value.split(';').next().unwrap_or(value).to_string())
                .unwrap_or_default();
        }
        let (frame_type, clock_rate, payload_type, ssrc) = match track {
            TrackInfo::Video(_) => (FrameType::Video, 90_000, 96, 0x5254_5301 + index as u32),
            TrackInfo::Audio(audio) => (
                FrameType::Audio,
                audio.sample_rate.max(1),
                97,
                0x5254_5301 + index as u32,
            ),
        };
        push_tracks.push(PushTrack {
            frame_type,
            channel,
            packetizer: RtpPacketizer::new(ssrc, payload_type, clock_rate),
        });
    }
    send_request(
        &mut socket,
        "RECORD",
        &target.uri,
        &mut cseq,
        &[
            ("Session", session.clone()),
            ("Range", "npt=0.000-".to_string()),
        ],
        &[],
    )
    .await?;
    ensure_success(read_response(&mut socket, &mut buffer).await?)?;
    let subscriber_id = format!("rtsp-push:{}", target.uri);
    info!(
        "RTSP push RECORD started: {vhost}/{app}/{stream_name} -> {}",
        target.uri
    );

    source.add_subscriber(subscriber_id.clone()).await;
    let mut receiver = source.subscribe();
    for frame in cached {
        send_frame(&mut socket, &mut push_tracks, &frame).await?;
    }
    loop {
        let frame = tokio::select! {
            _ = stop.notified() => break,
            frame = receiver.recv() => match frame {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        send_frame(&mut socket, &mut push_tracks, &frame).await?;
    }
    source.remove_subscriber(&subscriber_id).await;
    let _ = send_request(
        &mut socket,
        "TEARDOWN",
        &target.uri,
        &mut cseq,
        &[("Session", session)],
        &[],
    )
    .await;
    Ok(())
}

async fn send_frame(
    socket: &mut ClientStream,
    tracks: &mut [PushTrack],
    frame: &zlmediakit_core::MediaFrame,
) -> Result<()> {
    let Some(track) = tracks
        .iter_mut()
        .find(|track| track.frame_type == frame.frame_type)
    else {
        return Ok(());
    };
    for packet in track.packetizer.packetize(frame)? {
        let length = u16::try_from(packet.len()).context("RTSP RTP packet is too large")?;
        socket
            .write_all(&[b'$', track.channel, (length >> 8) as u8, length as u8])
            .await?;
        socket.write_all(&packet).await?;
    }
    Ok(())
}
