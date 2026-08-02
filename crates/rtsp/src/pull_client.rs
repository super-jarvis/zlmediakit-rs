use crate::client_transport::{connect, ClientStream};
use crate::parser::{RtspParser, RtspResponse};
use crate::session::{parse_sdp, RtpDepacketizer};
use bytes::{Buf, BytesMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tracing::{debug, error, info};
use zlmediakit_core::media_frame::{AudioInfo, CodecId, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::MediaSourceManager;

/// Pulls a remote RTSP stream (TCP-interleaved transport) and republishes it
/// locally under `(vhost, app, stream_name)` so it can be consumed by any
/// protocol this server supports. Runs until `stop` is signalled or the
/// upstream connection drops.
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    info!(
        "stream proxy: pulling {} -> {}/{}/{}",
        url, vhost, app, stream_name
    );
    let target = parse_rtsp_url(url)?;
    let mut stream = connect(&target.host, target.port, target.secure).await?;

    let source = source_manager.get_or_create(vhost, app, stream_name);

    let mut cseq = 1u32;
    let mut buffer = BytesMut::new();

    // OPTIONS
    send_request(&mut stream, "OPTIONS", &target.path, &mut cseq, &[]).await?;
    let _ = read_response(&mut stream, &mut buffer).await;

    // DESCRIBE
    send_request(
        &mut stream,
        "DESCRIBE",
        &target.path,
        &mut cseq,
        &[("Accept", "application/sdp")],
    )
    .await?;
    let resp = read_response(&mut stream, &mut buffer).await?;
    if resp.status_code != 200 {
        anyhow::bail!("DESCRIBE failed with status {}", resp.status_code);
    }
    let sdp = parse_sdp(&resp.body);
    let has_audio = String::from_utf8_lossy(&resp.body).contains("m=audio");

    // SETUP video track (interleaved 0-1)
    send_request(
        &mut stream,
        "SETUP",
        &format!("{}/trackID=0", target.path.trim_end_matches('/')),
        &mut cseq,
        &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
    )
    .await?;
    let setup_v = read_response(&mut stream, &mut buffer).await?;
    let session = setup_v
        .headers
        .get("Session")
        .map(|s| s.split(';').next().unwrap_or(s).to_string());

    if has_audio {
        let mut extra = vec![("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3")];
        if let Some(ref s) = session {
            extra.push(("Session", s));
        }
        send_request(
            &mut stream,
            "SETUP",
            &format!("{}/trackID=1", target.path.trim_end_matches('/')),
            &mut cseq,
            &extra,
        )
        .await?;
        let _ = read_response(&mut stream, &mut buffer).await;
    }

    // PLAY
    let mut play_extra = vec![("Range", "npt=0.000-")];
    if let Some(ref s) = session {
        play_extra.push(("Session", s));
    }
    send_request(&mut stream, "PLAY", &target.path, &mut cseq, &play_extra).await?;
    let play_resp = read_response(&mut stream, &mut buffer).await?;
    if play_resp.status_code != 200 {
        anyhow::bail!("PLAY failed with status {}", play_resp.status_code);
    }

    // Populate track info so media listings expose the correct codecs.
    {
        let mut info = source.info.write().await;
        info.tracks.clear();
        info.tracks.push(TrackInfo::Video(VideoInfo {
            codec: sdp.video_codec,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: false,
        }));
        if has_audio {
            info.tracks.push(TrackInfo::Audio(AudioInfo {
                codec: CodecId::AAC,
                sample_rate: sdp.audio_sample_rate,
                channels: 1,
                bits_per_sample: 16,
            }));
        }
    }

    let mut depak = RtpDepacketizer::new(
        source,
        sdp.video_pt,
        sdp.audio_pt,
        0, // video_chan (interleaved 0)
        2, // audio_chan (interleaved 2)
        sdp.audio_sample_rate,
        None,
        sdp.video_codec,
        sdp.sps,
        sdp.pps,
        sdp.vps,
        sdp.audio_config,
    );

    info!("stream proxy: PLAY ok, entering RTP loop for {}", url);
    let mut tmp = [0u8; 8192];
    loop {
        if stopped.load(Ordering::SeqCst) {
            info!("stream proxy: stop signal for {}", url);
            break;
        }
        tokio::select! {
            _ = stop.notified() => {
                if stopped.load(Ordering::SeqCst) {
                    info!("stream proxy: stop signal for {}", url);
                    break;
                }
            }
            n = stream.read(&mut tmp) => {
                match n {
                    Ok(0) => { info!("stream proxy: upstream closed {}", url); break; }
                    Ok(m) => buffer.extend_from_slice(&tmp[..m]),
                    Err(e) => { error!("stream proxy: read error {}: {}", url, e); break; }
                }
            }
        }
        // Drain whatever is currently buffered: interleaved RTP or stray RTSP
        // responses (e.g. RTCP/GET_PARAMETER replies) which we just discard.
        loop {
            if buffer.first() == Some(&b'$') {
                if buffer.len() < 4 {
                    break;
                }
                let channel = buffer[1];
                let plen = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
                if buffer.len() < 4 + plen {
                    break;
                }
                let payload = buffer[4..4 + plen].to_vec();
                buffer.advance(4 + plen);
                depak.handle_rtp(channel, &payload).await;
            } else if let Some((resp, consumed)) = RtspParser::parse_response(&buffer) {
                buffer.advance(consumed);
                debug!(
                    "stream proxy: received stray RTSP response {} (ignored)",
                    resp.status_code
                );
            } else {
                break;
            }
        }
    }

    // Best-effort TEARDOWN so the upstream frees the session promptly.
    if let Some(ref s) = session {
        let extra = vec![("Session", s.as_str())];
        let _ = send_request(&mut stream, "TEARDOWN", &target.path, &mut cseq, &extra).await;
    }
    Ok(())
}

struct PullUrl {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}

fn parse_rtsp_url(url: &str) -> anyhow::Result<PullUrl> {
    let (rest, secure, default_port) = if let Some(rest) = url.strip_prefix("rtsps://") {
        (rest, true, 322)
    } else if let Some(rest) = url.strip_prefix("rtsp://") {
        (rest, false, 554)
    } else {
        anyhow::bail!("not an RTSP URL: {url}");
    };
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |index| (&rest[..index], &rest[index..]));
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("invalid bracketed IPv6 RTSP host"))?;
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
    Ok(PullUrl {
        host,
        port,
        path: path.to_string(),
        secure,
    })
}

async fn send_request(
    stream: &mut ClientStream,
    method: &str,
    uri: &str,
    cseq: &mut u32,
    headers: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut req = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\n", method, uri, *cseq);
    *cseq += 1;
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    Ok(())
}

async fn read_response(
    stream: &mut ClientStream,
    buffer: &mut BytesMut,
) -> anyhow::Result<RtspResponse> {
    let mut tmp = [0u8; 4096];
    loop {
        if let Some((resp, consumed)) = RtspParser::parse_response(buffer) {
            buffer.advance(consumed);
            return Ok(resp);
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before a full RTSP response");
        }
        buffer.extend_from_slice(&tmp[..n]);
    }
}
