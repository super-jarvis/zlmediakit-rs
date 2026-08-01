use bytes::{Buf, Bytes, BytesMut};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_http::server::{HttpServer, HttpServerConfig};
use zlmediakit_http::ws::{compute_accept_key, WsFrame};

const TEST_PORT: u16 = 19126;

fn avcc_config() -> Bytes {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17, // FrameType=key(1)<<4 | CodecID=7
        0x00, // AVCPacketType=0 (AVCDecoderConfigurationRecord)
        0x00,
        0x00,
        0x00,     // CTS=0
        0x01,     // version
        0x42,     // profile
        0x00,     // compatible
        0x1f,     // level
        0xff,     // 6 bits reserved + 2 bits nal size - 1
        0xe0 | 1, // 3 bits reserved | numSPS
        0x00,
        0x0a, // SPS length
    ];
    v.extend_from_slice(&sps);
    v.push(0x01); // numPPS
    v.extend_from_slice(&[0x00, 0x04]); // PPS length
    v.extend_from_slice(&pps);
    Bytes::from(v)
}

fn avcc_sample(key: bool, seq: u32) -> Bytes {
    let nalu_type: u8 = if key { 0x65 } else { 0x41 };
    let nalu = [nalu_type, 0x9a, 0x00, 0x10 + (seq % 200) as u8, 0x20];
    let frame_type_byte: u8 = if key { 0x10 } else { 0x20 };
    let mut v = vec![
        frame_type_byte | 0x07, // FrameType|CodecID
        0x01,                   // AVCPacketType=1 (NAL unit)
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x05, // cts(3)+NALU length(4)=5
    ];
    v.extend_from_slice(&nalu);
    Bytes::from(v)
}

async fn http_raw(stream: &mut TcpStream, request: &[u8]) -> Vec<u8> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(request).await.unwrap();
    let mut buf = vec![];
    loop {
        let mut tmp = [0u8; 1];
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.push(tmp[0]);
        if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
    }
    buf
}

fn upgrade_request(port: u16, path: &str) -> String {
    let ws_key = "dGhlIHNhbXBsZSBub25jZQ==";
    format!(
        "GET {} HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        path, port, ws_key
    )
}

struct Fixture {
    mgr: Arc<MediaSourceManager>,
    port: u16,
}

async fn start_server(port: u16) -> Fixture {
    let (recorder, _recv) = RecorderControl::new();
    let (proxy, _table, _proxy_rx) = StreamProxyControl::new();
    let auth = StreamAuth::new(false, String::new());
    let hook = HookClient::empty();
    let mgr = Arc::new(MediaSourceManager::new(None));

    let srv = HttpServer::new(HttpServerConfig {
        addr: format!("127.0.0.1:{}", port),
        source_manager: mgr.clone(),
        auth,
        hook,
        recorder: Arc::new(recorder),
        proxy: Arc::new(proxy),
        pusher: Default::default(),
        ffmpeg: Default::default(),
        record_root: std::path::PathBuf::from("./record"),
        www_root: None,
        ssl_cert: None,
        ssl_key: None,
        rtp: None,
        sip: None,
        transcode: None,
        session_manager: None,
    })
    .await
    .expect("HttpServer should start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    Fixture { mgr, port }
}

async fn publish_stream(
    fx: &Fixture,
    app: &str,
    name: &str,
) -> Arc<zlmediakit_core::media_source::MediaSource> {
    let source = fx.mgr.get_or_create("__defaultVhost__", app, name);
    source
        .update_tracks(vec![TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: false,
        })])
        .await;

    source
        .publish_and_cache(MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            avcc_config(),
            true,
        ))
        .await;
    for i in 1..=5u32 {
        let is_key = i == 1;
        source
            .publish_and_cache(MediaFrame::new_video(
                0,
                CodecId::H264,
                i * 40,
                (i * 40) as u64,
                (i * 40) as u64,
                avcc_sample(is_key, i),
                is_key,
            ))
            .await;
    }
    source
}

/// Reads WS frames until `count` binary frames are received.
async fn read_frames(
    stream: &mut TcpStream,
    source: &Arc<zlmediakit_core::media_source::MediaSource>,
    target: u32,
) -> Vec<Vec<u8>> {
    let mut read_buf = BytesMut::with_capacity(65536);
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let result = timeout(Duration::from_secs(5), async {
        loop {
            if let Some((frame, consumed)) = WsFrame::decode(&read_buf[..]) {
                read_buf.advance(consumed);
                if frame.opcode == 0x2 {
                    frames.push(frame.payload.clone());
                    if frames.len() as u32 == target {
                        break;
                    }
                }
            } else {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                read_buf.extend_from_slice(&buf[..n]);
            }
        }
        frames.len()
    })
    .await;
    let _ = source;
    let _ = result.expect("timeout waiting for WS frames");
    frames
}

#[tokio::test]
async fn ws_ts_receives_mpeg_ts_over_websocket() {
    let fx = start_server(TEST_PORT).await;
    let source = publish_stream(&fx, "live", "tstest").await;
    let mut stream = TcpStream::connect(("127.0.0.1", fx.port))
        .await
        .expect("connect to server");

    let upgrade = upgrade_request(fx.port, "/live/tstest.live.ts");
    let resp = http_raw(&mut stream, upgrade.as_bytes()).await;
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(
        resp_str.contains("101"),
        "expected 101, got: {}",
        resp_str.lines().next().unwrap_or("")
    );
    assert!(
        resp_str.contains(&compute_accept_key("dGhlIHNhbXBsZSBub25jZQ==")),
        "missing Sec-WebSocket-Accept"
    );

    let frames = read_frames(&mut stream, &source, 5).await;
    assert!(
        frames.len() >= 5,
        "expected at least 5 binary frames (config + 4 gop samples), got {}",
        frames.len()
    );
    // Every WS-TS payload is a whole number of 188-byte TS packets.
    for f in &frames {
        assert_eq!(f.len() % 188, 0, "TS payload must be 188-byte aligned");
    }
    // The config frame output begins with a PAT/PMT, sync byte 0x47.
    assert_eq!(frames[0][0], 0x47, "TS stream must start with sync byte");

    // Live frame arrives over the subscription.
    source
        .publish(MediaFrame::new_video(
            0,
            CodecId::H264,
            300,
            300,
            300,
            avcc_sample(false, 42),
            false,
        ))
        .unwrap();

    let more = read_frames(&mut stream, &source, 1).await;
    assert!(
        !more.is_empty(),
        "expected at least one live TS frame, got {}",
        more.len()
    );

    let close = WsFrame::close(1000, "bye");
    use tokio::io::AsyncWriteExt;
    let _ = stream.write_all(&close.encode_server()).await;
    source.close();
}

#[tokio::test]
async fn ws_fmp4_receives_init_segment_and_media_segments() {
    let fx = start_server(TEST_PORT + 1).await;
    let source = publish_stream(&fx, "live", "mp4test").await;

    let mut stream = TcpStream::connect(("127.0.0.1", fx.port))
        .await
        .expect("connect to server");

    let upgrade = upgrade_request(fx.port, "/live/mp4test.live.mp4");
    let resp = http_raw(&mut stream, upgrade.as_bytes()).await;
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(
        resp_str.contains("101"),
        "expected 101, got: {}",
        resp_str.lines().next().unwrap_or("")
    );

    let frames = read_frames(&mut stream, &source, 2).await;
    assert!(
        frames.len() >= 2,
        "expected at least 2 binary frames (init + media segment), got {}",
        frames.len()
    );
    // First frame is the initialization segment: starts with ftyp.
    assert_eq!(
        &frames[0][4..8],
        b"ftyp",
        "init segment must start with ftyp"
    );
    // Media segments begin with styp (CMAF) or moof.
    let media = &frames[1];
    let brand = &media[4..8];
    assert!(
        brand == b"styp" || brand == b"moof",
        "media segment must start with styp/moof, got {:?}",
        brand
    );

    let close = WsFrame::close(1000, "bye");
    use tokio::io::AsyncWriteExt;
    let _ = stream.write_all(&close.encode_server()).await;
    source.close();
}
