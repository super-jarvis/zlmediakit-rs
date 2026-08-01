use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::rtsp_pull_start;
use zlmediakit_rtsp::RtspServer;

const UPSTREAM_PORT: u16 = 19147;
const UPSTREAM_ADDR: &str = "127.0.0.1:19147";

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

fn make_rtp_header(pt: u8, seq: u16, ts: u32, ssrc: u32, marker: bool) -> [u8; 12] {
    let m_bit = if marker { 0x80 } else { 0x00 };
    [
        0x80,
        m_bit | (pt & 0x7F),
        (seq >> 8) as u8,
        seq as u8,
        (ts >> 24) as u8,
        (ts >> 16) as u8,
        (ts >> 8) as u8,
        ts as u8,
        (ssrc >> 24) as u8,
        (ssrc >> 16) as u8,
        (ssrc >> 8) as u8,
        ssrc as u8,
    ]
}

fn make_rtp_packet(nalu: &[u8], pt: u8, seq: u16, ts: u32, ssrc: u32, marker: bool) -> Vec<u8> {
    let mut pkt = make_rtp_header(pt, seq, ts, ssrc, marker).to_vec();
    pkt.extend_from_slice(nalu);
    pkt
}

async fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    loop {
        let s = String::from_utf8_lossy(buf);
        if let Some(pos) = s.find("\r\n\r\n") {
            let header_end = pos + 4;
            let cl = s[..pos]
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= header_end + cl {
                return;
            }
        }
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn rtsp_request(stream: &mut TcpStream, request: &[u8], buf: &mut Vec<u8>) -> u16 {
    stream.write_all(request).await.unwrap();
    read_response(stream, buf).await;
    let s = String::from_utf8_lossy(buf);
    let pos = s.find("\r\n\r\n").unwrap_or(0);
    let code = s
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1)?.parse().ok())
        .unwrap_or(0);
    let cl = s[..pos]
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total = pos + 4 + cl;
    buf.drain(..total.min(buf.len()));
    code
}

fn header_value(resp: &str, name: &str) -> Option<String> {
    resp.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(name) {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

async fn rtsp_request_full(
    stream: &mut TcpStream,
    request: &[u8],
    buf: &mut Vec<u8>,
) -> (u16, String) {
    stream.write_all(request).await.unwrap();
    read_response(stream, buf).await;
    let s = String::from_utf8_lossy(buf);
    let pos = s.find("\r\n\r\n").unwrap_or(0);
    let code = s
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1)?.parse().ok())
        .unwrap_or(0);
    let cl = s[..pos]
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total = pos + 4 + cl;
    let resp = s[..total].to_string();
    buf.drain(..total.min(buf.len()));
    (code, resp)
}

#[tokio::test]
async fn rtsp_pull_proxy_receives_media() {
    let upstream_mgr = Arc::new(MediaSourceManager::new(None));
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(false, String::new());

    // Start upstream RTSP server
    let srv = RtspServer::new(
        UPSTREAM_ADDR,
        upstream_mgr.clone(),
        event_bus,
        auth,
        None,
        None,
        None,
    )
    .await
    .expect("upstream server start");
    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Publisher: connect and publish to upstream ---
    let mut pub_stream = TcpStream::connect(("127.0.0.1", UPSTREAM_PORT))
        .await
        .expect("publisher connect");
    pub_stream.set_nodelay(true).unwrap();
    let mut buf = Vec::new();

    // OPTIONS
    rtsp_request(
        &mut pub_stream,
        b"OPTIONS rtsp://127.0.0.1/live/source RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        &mut buf,
    )
    .await;

    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];

    // ANNOUNCE
    let sdp = make_sdp(&sps, &pps);
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1:{}/live/source RTSP/1.0\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
        UPSTREAM_PORT, sdp.len(), sdp
    );
    rtsp_request(&mut pub_stream, announce.as_bytes(), &mut buf).await;

    // SETUP
    let setup = format!(
        "SETUP rtsp://127.0.0.1:{}/live/source/track1 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n",
        UPSTREAM_PORT
    );
    let (code, resp) = rtsp_request_full(&mut pub_stream, setup.as_bytes(), &mut buf).await;
    assert_eq!(code, 200, "SETUP");
    let session_id = header_value(&resp, "Session").unwrap_or_default();
    assert!(!session_id.is_empty());

    // RECORD
    let record = format!(
        "RECORD rtsp://127.0.0.1:{}/live/source RTSP/1.0\r\nCSeq: 4\r\nSession: {}\r\n\r\n",
        UPSTREAM_PORT, session_id
    );
    let code = rtsp_request(&mut pub_stream, record.as_bytes(), &mut buf).await;
    assert_eq!(code, 200, "RECORD");

    // --- Start pull proxy ---
    let downstream_mgr = Arc::new(MediaSourceManager::new(None));
    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));

    let pull_url = format!("rtsp://127.0.0.1:{}/live/source", UPSTREAM_PORT);
    let pull_mgr = downstream_mgr.clone();
    let pull_stop = stop.clone();
    let pull_stopped = stopped.clone();
    tokio::spawn(async move {
        let _ = rtsp_pull_start(
            &pull_url,
            "__defaultVhost__",
            "live",
            "pulled",
            pull_mgr,
            pull_stop,
            pull_stopped,
        )
        .await;
    });

    // --- Publisher sends frames ---
    let ssrc = 12345u32;
    let pt = 96u8;

    // SPS (type 7)
    let nalu = make_rtp_packet(&sps, pt, 0, 0, ssrc, true);
    pub_stream
        .write_all(&make_interleaved_frame(0, &nalu))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // PPS (type 8)
    let nalu = make_rtp_packet(&pps, pt, 1, 3000, ssrc, true);
    pub_stream
        .write_all(&make_interleaved_frame(0, &nalu))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // IDR (type 5)
    let idr = [0x65u8, 0x9a, 0x00, 0x15, 0x20];
    let nalu = make_rtp_packet(&idr, pt, 2, 6000, ssrc, true);
    pub_stream
        .write_all(&make_interleaved_frame(0, &nalu))
        .await
        .unwrap();

    // Wait for pull client to connect, SETUP, PLAY, and receive frames.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify frames arrived in downstream
    let source = downstream_mgr.get("__defaultVhost__", "live", "pulled");
    assert!(source.is_some(), "pulled source should exist");
    let source = source.unwrap();

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(
        frames.len() >= 2,
        "downstream should have at least 2 cached frames (config + sample), got {}",
        frames.len()
    );
    drop(cached);

    // Cleanup
    stop.notify_waiters();
    let teardown = format!(
        "TEARDOWN rtsp://127.0.0.1:{}/live/source RTSP/1.0\r\nCSeq: 5\r\nSession: {}\r\n\r\n",
        UPSTREAM_PORT, session_id
    );
    let _ = pub_stream.write_all(teardown.as_bytes()).await;
}
