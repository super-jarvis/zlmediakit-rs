//! E2E tests for RTSP Basic authentication in the pull/push clients.
//!
//! A mock upstream RTSP server challenges every negotiation request with
//! `WWW-Authenticate: Basic realm=...` until the client retries with a valid
//! `Authorization: Basic base64(user:pass)` header.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};
use zlmediakit_core::media_frame::{CodecId, MediaFrame, PayloadFormat, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::{push_client, rtsp_pull_start};

const USER: &str = "publisher";
const PASS: &str = "s3cret";
const REALM: &str = "rtsp-test";

fn expected_basic() -> String {
    let token = BASE64.encode(format!("{USER}:{PASS}"));
    format!("Basic {token}")
}

fn make_sdp(sps: &[u8], pps: &[u8]) -> String {
    let sps_b64 = BASE64.encode(sps);
    let pps_b64 = BASE64.encode(pps);
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=RTSP test\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode=1;sprop-parameter-sets={},{}\r\n",
        sps_b64, pps_b64
    )
}

fn make_interleaved_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.push(b'$');
    frame.push(channel);
    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn make_rtp_packet(nalu: &[u8], pt: u8, seq: u16, ts: u32, ssrc: u32) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + nalu.len());
    pkt.extend_from_slice(&[0x80, 0x80 | (pt & 0x7F), (seq >> 8) as u8, seq as u8]);
    pkt.extend_from_slice(&ts.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(nalu);
    pkt
}

struct RtspRequest {
    method: String,
    headers: HashMap<String, String>,
}

/// Read one RTSP request (headers + Content-Length body) from the wire.
async fn read_request(stream: &mut TcpStream) -> Option<RtspRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == &b"\r\n\r\n"[..]) {
            let header_end = pos + 4;
            let text = String::from_utf8_lossy(&buf[..pos]).to_string();
            let mut lines = text.lines();
            let start = lines.next().unwrap_or("").to_string();
            let method = match start.split_whitespace().collect::<Vec<_>>()[..] {
                [m, _, _] => m.to_string(),
                _ => return None,
            };
            let mut headers = HashMap::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let value = v.trim().to_string();
                    if key == "content-length" {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.insert(key, value);
                }
            }
            while buf.len() < header_end + content_length {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            return Some(RtspRequest { method, headers });
        }
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn respond(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    cseq: Option<&str>,
    extra: &[(&str, &str)],
    body: &[u8],
) {
    let mut response = format!("RTSP/1.0 {code} {reason}\r\n");
    if let Some(cseq) = cseq {
        response.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    for (name, value) in extra {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await.unwrap();
    if !body.is_empty() {
        stream.write_all(body).await.unwrap();
    }
}

/// Read `want` RFC2326 interleaved `$channel len payload` frames from the wire.
async fn read_interleaved(stream: &mut TcpStream, want: usize, timeout: Duration) -> Vec<Vec<u8>> {
    let mut buf = Vec::new();
    let mut frames = Vec::new();
    let mut tmp = [0u8; 8192];
    let deadline = Instant::now() + timeout;
    loop {
        if frames.len() >= want {
            break;
        }
        if buf.first() == Some(&b'$') {
            if buf.len() < 4 {
                let n = match tokio::time::timeout(
                    deadline.saturating_duration_since(Instant::now()),
                    stream.read(&mut tmp),
                )
                .await
                {
                    Ok(Ok(n)) => n,
                    _ => break,
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                continue;
            }
            let plen = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            if buf.len() < 4 + plen {
                let n = match tokio::time::timeout(
                    deadline.saturating_duration_since(Instant::now()),
                    stream.read(&mut tmp),
                )
                .await
                {
                    Ok(Ok(n)) => n,
                    _ => break,
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                continue;
            }
            let payload = buf[4..4 + plen].to_vec();
            buf.drain(..4 + plen);
            frames.push(payload);
        } else {
            let n = match tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                stream.read(&mut tmp),
            )
            .await
            {
                Ok(Ok(n)) => n,
                _ => break,
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }
    frames
}

#[tokio::test]
async fn rtsp_pull_retries_with_basic_auth_and_receives_media() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();

    // --- Mock upstream: Basic-challenge on DESCRIBE, then serve video ---
    let mock = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let basic = expected_basic();

        loop {
            let Some(request) = read_request(&mut stream).await else {
                break;
            };
            let cseq = request.headers.get("cseq").map(String::as_str);
            match request.method.as_str() {
                "OPTIONS" => {
                    respond(&mut stream, 200, "OK", cseq, &[], b"").await;
                }
                "DESCRIBE" => {
                    let authorized = request
                        .headers
                        .get("authorization")
                        .map(|value| value == &basic)
                        .unwrap_or(false);
                    if !authorized {
                        respond(
                            &mut stream,
                            401,
                            "Unauthorized",
                            cseq,
                            &[("WWW-Authenticate", &format!("Basic realm=\"{REALM}\""))],
                            b"",
                        )
                        .await;
                    } else {
                        let sdp = make_sdp(&[0x67, 0x42], &[0x68, 0xee]);
                        respond(&mut stream, 200, "OK", cseq, &[], sdp.as_bytes()).await;
                    }
                }
                "SETUP" => {
                    respond(
                        &mut stream,
                        200,
                        "OK",
                        cseq,
                        &[("Session", "sess-basic-pull;timeout=60")],
                        b"",
                    )
                    .await;
                }
                "PLAY" => {
                    respond(
                        &mut stream,
                        200,
                        "OK",
                        cseq,
                        &[("Session", "sess-basic-pull")],
                        b"",
                    )
                    .await;
                }
                "TEARDOWN" => {
                    respond(&mut stream, 200, "OK", cseq, &[], b"").await;
                    break;
                }
                _ => {
                    respond(&mut stream, 200, "OK", cseq, &[], b"").await;
                }
            }
            if request.method == "PLAY" {
                break;
            }
        }

        // After PLAY, stream SPS/PPS/IDR as interleaved RTP on channel 0.
        let ssrc = 0xA11CEu32;
        let pt = 96u8;
        let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
        let pps = [0x68u8, 0xee, 0x3c, 0x80];
        let idr = [0x65u8, 0x9a, 0x00, 0x15, 0x20];
        for (index, nalu) in [&sps[..], &pps[..], &idr[..]].iter().enumerate() {
            let seq = index as u16;
            let packet = make_rtp_packet(nalu, pt, seq, index as u32 * 3000, ssrc);
            stream
                .write_all(&make_interleaved_frame(0, &packet))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Let the client drain, then close so it exits the read loop.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    // --- Run the pull client against the mock ---
    let downstream_mgr = Arc::new(MediaSourceManager::new(None));
    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));

    let pull_url = format!("rtsp://{USER}:{PASS}@127.0.0.1:{upstream_port}/live/basic");
    let pull_mgr = downstream_mgr.clone();
    let pull_stop = stop.clone();
    let pull_stopped = stopped.clone();
    let pull_task = tokio::spawn(async move {
        rtsp_pull_start(
            &pull_url,
            "__defaultVhost__",
            "live",
            "basic-pulled",
            pull_mgr,
            pull_stop,
            pull_stopped,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(source) = downstream_mgr.get("__defaultVhost__", "live", "basic-pulled") {
                let frames = source.get_latest_gop_frames().await;
                if frames
                    .iter()
                    .any(|frame| frame.codec == CodecId::H264 && !frame.config_frame)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pull client did not receive media over Basic auth");

    stop.notify_waiters();
    stopped.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(2), pull_task)
        .await
        .map(|result| result.ok());
    mock.await.unwrap();
}

#[tokio::test]
async fn rtsp_push_retries_with_basic_auth_and_forwards_media() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();

    // --- Mock upstream: Basic-challenge on ANNOUNCE, then collect RTP ---
    let mock = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let basic = expected_basic();
        let session = "sess-basic-push".to_string();

        loop {
            let Some(request) = read_request(&mut stream).await else {
                break;
            };
            let cseq = request.headers.get("cseq").map(String::as_str);
            match request.method.as_str() {
                "ANNOUNCE" => {
                    let authorized = request
                        .headers
                        .get("authorization")
                        .map(|value| value == &basic)
                        .unwrap_or(false);
                    if !authorized {
                        respond(
                            &mut stream,
                            401,
                            "Unauthorized",
                            cseq,
                            &[("WWW-Authenticate", &format!("Basic realm=\"{REALM}\""))],
                            b"",
                        )
                        .await;
                    } else {
                        respond(&mut stream, 200, "OK", cseq, &[("Session", &session)], b"").await;
                    }
                }
                "SETUP" => {
                    respond(&mut stream, 200, "OK", cseq, &[("Session", &session)], b"").await;
                }
                "RECORD" => {
                    respond(&mut stream, 200, "OK", cseq, &[("Session", &session)], b"").await;
                }
                "TEARDOWN" => {
                    respond(&mut stream, 200, "OK", cseq, &[], b"").await;
                    break;
                }
                _ => {
                    respond(&mut stream, 200, "OK", cseq, &[], b"").await;
                }
            }
            if request.method == "RECORD" {
                break;
            }
        }

        // The push client sends the cached GOP immediately; expect RTP frames.
        let frames = read_interleaved(&mut stream, 2, Duration::from_secs(3)).await;
        frames
    });

    // --- Local source to push ---
    let local_manager = Arc::new(MediaSourceManager::new(None));
    let source = local_manager.get_or_create("__defaultVhost__", "live", "basic-camera");
    let mut source_info = source.info.write().await;
    source_info.tracks.push(TrackInfo::Video(VideoInfo {
        codec: CodecId::H264,
        width: 1280,
        height: 720,
        fps: 25.0,
        key_frame: true,
    }));
    drop(source_info);
    let mut config = MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        0,
        0,
        Bytes::from_static(&[
            0x17, 0, 0, 0, 0, 0x01, 0x64, 0, 0x0a, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x0a, 1, 0, 4,
            0x68, 0xee, 0x3c, 0x80,
        ]),
        false,
    )
    .with_payload_format(PayloadFormat::Flv);
    config.config_frame = true;
    source.publish_and_cache(config).await;
    source
        .publish_and_cache(
            MediaFrame::new_video(
                0,
                CodecId::H264,
                40,
                40,
                40,
                Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3, 4]),
                true,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        )
        .await;

    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let push_stop = stop.clone();
    let push_stopped = stopped.clone();
    let push_manager = local_manager.clone();
    let push_task = tokio::spawn(async move {
        push_client::start(
            &format!("rtsp://{USER}:{PASS}@127.0.0.1:{upstream_port}/live/forwarded"),
            "__defaultVhost__",
            "live",
            "basic-camera",
            push_manager,
            push_stop,
            push_stopped,
        )
        .await
    });

    let frames = tokio::time::timeout(Duration::from_secs(4), mock)
        .await
        .expect("push client never reached RECORD")
        .unwrap();
    assert!(
        frames.len() >= 2,
        "mock upstream should receive at least 2 interleaved RTP frames, got {}",
        frames.len()
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.len() >= 13 && frame[1] == 96),
        "expected H.264 RTP (PT 96) among pushed frames"
    );

    stopped.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    let _ = tokio::time::timeout(Duration::from_secs(2), push_task)
        .await
        .map(|result| result.ok());
}

#[tokio::test]
async fn rtsp_push_basic_auth_rejects_wrong_password() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();

    // --- Mock upstream: always 401 Basic, never accepts ---
    let mock = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        while let Some(request) = read_request(&mut stream).await {
            let cseq = request.headers.get("cseq").map(String::as_str);
            respond(
                &mut stream,
                401,
                "Unauthorized",
                cseq,
                &[("WWW-Authenticate", &format!("Basic realm=\"{REALM}\""))],
                b"",
            )
            .await;
        }
    });

    let local_manager = Arc::new(MediaSourceManager::new(None));
    let source = local_manager.get_or_create("__defaultVhost__", "live", "wrong-cred");
    let mut source_info = source.info.write().await;
    source_info.tracks.push(TrackInfo::Video(VideoInfo {
        codec: CodecId::H264,
        width: 1280,
        height: 720,
        fps: 25.0,
        key_frame: true,
    }));
    drop(source_info);

    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let push_stop = stop.clone();
    let push_stopped = stopped.clone();
    let push_manager = local_manager.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        push_client::start(
            &format!("rtsp://{USER}:wrong@127.0.0.1:{upstream_port}/live/forwarded"),
            "__defaultVhost__",
            "live",
            "wrong-cred",
            push_manager,
            push_stop,
            push_stopped,
        ),
    )
    .await
    .expect("push client should fail promptly")
    .expect_err("push client must report an authentication failure");

    assert!(
        result.to_string().contains("401"),
        "expected 401 in error, got: {result}"
    );
    mock.await.unwrap();
}
