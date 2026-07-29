use bytes::Bytes;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_http::server::HttpServer;

const TEST_PORT: u16 = 19146;

fn avcc_config() -> Bytes {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17,
        0x00,
        0x00,
        0x00,
        0x00,
        0x01,
        0x42,
        0x00,
        0x1f,
        0xff,
        0xe0 | 1,
        0x00,
        0x0a,
    ];
    v.extend_from_slice(&sps);
    v.push(0x01);
    v.extend_from_slice(&[0x00, 0x04]);
    v.extend_from_slice(&pps);
    Bytes::from(v)
}

fn avcc_sample(key: bool, data: &[u8]) -> Bytes {
    let ft = if key { 0x17 } else { 0x27 };
    let mut v = vec![ft, 0x01, 0x00, 0x00, 0x00];
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    Bytes::from(v)
}

/// Send HTTP request and return (status_code, body).
async fn http_get(path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("connect");
    let request = format!("GET {} HTTP/1.0\r\nHost: localhost\r\n\r\n", path);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let s = String::from_utf8_lossy(&buf);
    let pos = s.find("\r\n").unwrap_or(0);
    let status_line = &s[..pos];
    let code = status_line
        .splitn(3, ' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body_start = s.find("\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
    let body = buf[body_start..].to_vec();
    (code, body)
}

#[tokio::test]
async fn hls_e2e_generates_playlist_and_segments() {
    let mgr = Arc::new(MediaSourceManager::new());
    let auth = StreamAuth::new(false, String::new());
    let recorder = Arc::new(RecorderControl::new().0);
    let proxy = Arc::new(StreamProxyControl::new().0);

    let addr = format!("127.0.0.1:{}", TEST_PORT);
    let srv = HttpServer::new(&addr, mgr.clone(), auth, recorder, proxy, None, None)
        .await
        .expect("HttpServer should start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create the media source with a config frame so it exists.
    let source = mgr.get_or_create("__defaultVhost__", "live", "test");

    // Config frame (SPS/PPS)
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config(), false);
    source.publish_and_cache(cfg).await;

    // Request the playlist — this triggers the background HLS muxer task.
    // The config frame will be replayed into the muxer (starting its segment
    // timer).
    let (code, _) = http_get("/live/test/hls.m3u8").await;
    assert_eq!(code, 200, "playlist request should succeed");

    // Push frames with real-time spacing > 2 s to trigger segment flush.
    // First frame (P-frame, starts timer if not already started):
    let nalu = [0x41u8, 0x9a, 0x00, 0x15, 0x20];
    let f = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_sample(false, &nalu), false);
    source.publish_and_cache(f).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Second frame (P-frame):
    let nalu = [0x41u8, 0x9a, 0x01, 0x15, 0x20];
    let f = MediaFrame::new_video(
        0,
        CodecId::H264,
        500,
        500,
        500,
        avcc_sample(false, &nalu),
        false,
    );
    source.publish_and_cache(f).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Third frame (IDR key frame, should trigger flush since >2 s elapsed):
    let nalu = [0x65u8, 0x9a, 0x02, 0x15, 0x20];
    let f = MediaFrame::new_video(
        0,
        CodecId::H264,
        1000,
        1000,
        1000,
        avcc_sample(true, &nalu),
        true,
    );
    source.publish_and_cache(f).await;

    // Wait for the segment to be produced.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Fetch playlist — should now list at least one segment.
    let (code, body) = http_get("/live/test/hls.m3u8").await;
    assert_eq!(code, 200, "second playlist request should succeed");
    let body_str = String::from_utf8_lossy(&body);

    assert!(
        body_str.contains(".ts"),
        "playlist should contain TS segments: {}",
        body_str
    );

    // Extract first .ts filename from the playlist
    let ts_name = body_str
        .lines()
        .find(|l| l.trim().ends_with(".ts") && l.trim().starts_with('/'))
        .expect("playlist should have a .ts line")
        .trim();
    assert!(!ts_name.is_empty(), "TS path should not be empty");

    // Fetch the TS segment
    let (code, ts_body) = http_get(ts_name).await;
    assert_eq!(code, 200, "TS segment request should succeed");
    assert!(!ts_body.is_empty(), "TS segment body should not be empty");
    assert_eq!(
        ts_body[0], 0x47,
        "TS segment should start with sync byte 0x47"
    );

    // Verify multiple TS packets (each 188 bytes)
    let ts_packets = ts_body.len() / 188;
    assert!(
        ts_packets >= 2,
        "TS segment should contain at least 2 packets, got {}",
        ts_packets
    );
}
