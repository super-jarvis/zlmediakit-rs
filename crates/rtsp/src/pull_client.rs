use crate::parser::{RtspParser, RtspResponse};
use crate::session::{parse_sdp, RtpDepacketizer};
use bytes::{Buf, BytesMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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
    let (host, port, path) = parse_rtsp_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("connect {}:{} failed: {}", host, port, e))?;

    let source = source_manager.get_or_create(vhost, app, stream_name);

    let mut cseq = 1u32;
    let mut buffer = BytesMut::new();

    // OPTIONS
    send_request(&mut stream, "OPTIONS", &path, &mut cseq, &[]).await?;
    let _ = read_response(&mut stream, &mut buffer).await;

    // DESCRIBE
    send_request(
        &mut stream,
        "DESCRIBE",
        &path,
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
        &format!("{}/trackID=0", path),
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
            &format!("{}/trackID=1", path),
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
    send_request(&mut stream, "PLAY", &path, &mut cseq, &play_extra).await?;
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
        let _ = send_request(&mut stream, "TEARDOWN", &path, &mut cseq, &extra).await;
    }
    Ok(())
}

fn parse_rtsp_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let s = url
        .strip_prefix("rtsp://")
        .or_else(|| url.strip_prefix("rtsps://"))
        .ok_or_else(|| anyhow::anyhow!("not an rtsp url: {}", url))?;
    let (authority, path) = match s.find('/') {
        Some(i) => (s[..i].to_string(), format!("/{}", &s[i + 1..])),
        None => (s.to_string(), "/".to_string()),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let p = authority[i + 1..].parse::<u16>().unwrap_or(554);
            (authority[..i].to_string(), p)
        }
        None => (authority, 554),
    };
    Ok((host, port, path))
}

async fn send_request(
    stream: &mut TcpStream,
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
    stream: &mut TcpStream,
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
