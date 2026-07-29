use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::RtspServer;

const TEST_PORT: u16 = 19154;

fn make_hevc_sdp(vps: &[u8], sps: &[u8], pps: &[u8]) -> String {
    let vps_b64 = BASE64.encode(vps);
    let sps_b64 = BASE64.encode(sps);
    let pps_b64 = BASE64.encode(pps);
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=RTSP HEVC test\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H265/90000\r\n\
         a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
        vps_b64, sps_b64, pps_b64
    )
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

fn make_interleaved_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.push(b'$');
    frame.push(channel);
    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

async fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>) {
    loop {
        let s = String::from_utf8_lossy(buf);
        if let Some(pos) = s.find("\r\n\r\n") {
            let header_end = pos + 4;
            let content_length = s[..pos]
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let total_needed = header_end + content_length;
            if buf.len() >= total_needed {
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

async fn rtsp_request(stream: &mut TcpStream, request: &[u8], buf: &mut Vec<u8>) -> (u16, String) {
    stream.write_all(request).await.unwrap();
    read_response(stream, buf).await;

    let s = String::from_utf8_lossy(buf);
    let pos = s.find("\r\n\r\n").unwrap_or(0);
    let header_end = pos + 4;
    let status_line = s.lines().next().unwrap_or("");
    let code: u16 = status_line
        .splitn(3, ' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);

    let content_length = s[..pos]
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let total_len = header_end + content_length;
    let resp_str = s[..total_len].to_string();
    buf.drain(..total_len);
    (code, resp_str)
}

fn drain_interleaved(buf: &mut Vec<u8>) {
    while buf.len() >= 4 && buf[0] == b'$' {
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if buf.len() < 4 + len {
            break;
        }
        buf.drain(..4 + len);
    }
}

fn header_value(resp: &str, name: &str) -> Option<String> {
    resp.lines().find_map(|l| {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
        None
    })
}

#[tokio::test]
async fn rtsp_hevc_e2e_publish_receives_media() {
    let mgr = Arc::new(MediaSourceManager::new());
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(false, String::new());

    let addr = format!("127.0.0.1:{}", TEST_PORT);
    let srv = RtspServer::new(&addr, mgr.clone(), event_bus, auth, None, None)
        .await
        .expect("RtspServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("connect to RTSP server");
    stream.set_nodelay(true).unwrap();
    let mut buf = Vec::new();

    // OPTIONS
    let (code, _) = rtsp_request(
        &mut stream,
        b"OPTIONS rtsp://127.0.0.1/live/hevctest RTSP/1.0\r\nCSeq: 1\r\n\r\n",
        &mut buf,
    )
    .await;
    assert_eq!(code, 200, "OPTIONS failed");

    // HEVC parameter sets
    let vps = [
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x55, 0xac, 0x09,
    ];
    let sps = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x55, 0xa0, 0x02, 0x80, 0x80, 0x5d, 0x1f, 0xc9, 0x58, 0x00, 0x00, 0x75, 0x94,
        0xd9, 0x0b, 0x50, 0xbb,
    ];
    let pps = [0x44, 0x01, 0xc0, 0x72, 0x90, 0x04];

    // ANNOUNCE
    let sdp = make_hevc_sdp(&vps, &sps, &pps);
    let announce = format!(
        "ANNOUNCE rtsp://127.0.0.1:{}/live/hevctest RTSP/1.0\r\n\
         CSeq: 2\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{}",
        TEST_PORT,
        sdp.len(),
        sdp
    );
    let (code, _) = rtsp_request(&mut stream, announce.as_bytes(), &mut buf).await;
    assert_eq!(code, 200, "ANNOUNCE failed");

    // SETUP
    let setup = format!(
        "SETUP rtsp://127.0.0.1:{}/live/hevctest/track1 RTSP/1.0\r\n\
         CSeq: 3\r\n\
         Transport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n",
        TEST_PORT
    );
    let (code, resp) = rtsp_request(&mut stream, setup.as_bytes(), &mut buf).await;
    assert_eq!(code, 200, "SETUP failed");
    let session_id = header_value(&resp, "Session").unwrap_or_default();
    assert!(
        !session_id.is_empty(),
        "SETUP should provide Session header"
    );

    // RECORD
    let record = format!(
        "RECORD rtsp://127.0.0.1:{}/live/hevctest RTSP/1.0\r\n\
         CSeq: 4\r\n\
         Session: {}\r\n\r\n",
        TEST_PORT, session_id
    );
    let (code, _) = rtsp_request(&mut stream, record.as_bytes(), &mut buf).await;
    assert_eq!(code, 200, "RECORD failed");

    // Drain any interleaved data
    drain_interleaved(&mut buf);

    let ssrc = 12345u32;
    let pt = 96u8;
    let mut ts = 0u32;

    // Send VPS NALU (type 32, first byte = 0x40) via RTP
    let nalu_vps = make_rtp_packet(&vps, pt, 0, ts, ssrc, true);
    stream
        .write_all(&make_interleaved_frame(0, &nalu_vps))
        .await
        .unwrap();

    ts += 3000;

    // Send SPS NALU (type 33, first byte = 0x42) via RTP
    let nalu_sps = make_rtp_packet(&sps, pt, 1, ts, ssrc, true);
    stream
        .write_all(&make_interleaved_frame(0, &nalu_sps))
        .await
        .unwrap();

    ts += 3000;

    // Send PPS NALU (type 34, first byte = 0x44) via RTP
    let nalu_pps = make_rtp_packet(&pps, pt, 2, ts, ssrc, true);
    stream
        .write_all(&make_interleaved_frame(0, &nalu_pps))
        .await
        .unwrap();

    ts += 3000;

    // Send HEVC IDR NALU (type 19, first byte = 0x26 = 19<<1) via RTP
    let idr = [0x26u8, 0x01, 0x02, 0x03, 0x04];
    let nalu_idr = make_rtp_packet(&idr, pt, 3, ts, ssrc, true);
    stream
        .write_all(&make_interleaved_frame(0, &nalu_idr))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify via MediaSourceManager
    let source = mgr.get("__defaultVhost__", "live", "hevctest");
    assert!(
        source.is_some(),
        "source should exist after RTSP HEVC publish"
    );
    let source = source.unwrap();

    let info = source.info.read().await;
    let has_video = info
        .tracks
        .iter()
        .any(|t| matches!(t, zlmediakit_core::media_frame::TrackInfo::Video(_)));
    assert!(has_video, "should have video track info");
    drop(info);

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(
        frames.len() >= 2,
        "should have at least 2 cached frames (config + sample), got {}",
        frames.len()
    );
    let first = &frames[0];
    assert!(
        zlmediakit_core::gop_cache::is_config_frame(first),
        "first frame should be video config"
    );
    assert_eq!(
        first.codec,
        zlmediakit_core::media_frame::CodecId::H265,
        "codec should be H265 for HEVC stream"
    );
    drop(cached);

    // TEARDOWN
    let teardown = format!(
        "TEARDOWN rtsp://127.0.0.1:{}/live/hevctest RTSP/1.0\r\n\
         CSeq: 5\r\n\
         Session: {}\r\n\r\n",
        TEST_PORT, session_id
    );
    let _ = stream.write_all(teardown.as_bytes()).await;
    source.close();
}
