use bytes::{Buf, Bytes, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

const TEST_PORT: u16 = 19125;

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
    // FLV VideoTagHeader: FrameType(4b)|CodecID(4b) then AVCPacketType
    // FrameType: 1=key, 2=inter, CodecID: 7=H264
    let frame_type_byte: u8 = if key { 0x10 } else { 0x20 };
    let mut v = vec![
        frame_type_byte | 0x07, // FrameType|CodecID, 0x17=key, 0x27=inter
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

/// Sends an HTTP/1.1 request and returns the raw response up to the end of
/// headers (`\r\n\r\n`).  After the 101 Switching Protocols handshake the
/// server immediately starts sending binary WebSocket frames on the same
/// connection, so we must stop reading at the header boundary.
async fn http_raw(stream: &mut TcpStream, request: &[u8]) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    stream.write_all(request).await.unwrap();
    let mut buf = vec![];
    loop {
        let mut tmp = [0u8; 1];
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.push(tmp[0]);
        // End of headers.
        if buf.len() >= 4 && buf[buf.len() - 4..] == [b'\r', b'\n', b'\r', b'\n'] {
            break;
        }
    }
    buf
}

#[tokio::test]
async fn wsflv_e2e_receives_flv_over_websocket() {
    let (recorder, _recv) = RecorderControl::new();
    let (proxy, _table, _proxy_rx) = StreamProxyControl::new();
    let auth = StreamAuth::new(false, String::new());
    let hook = HookClient::empty();
    let mgr = Arc::new(MediaSourceManager::new());

    let hook = HookClient::empty();
    let addr = format!("127.0.0.1:{}", TEST_PORT);
    let srv = HttpServer::new(HttpServerConfig {
        addr,
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
    })
    .await
    .expect("HttpServer should start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    // Give the server a moment to start.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Publish a synthetic H.264 stream.
    let source = mgr.get_or_create("__defaultVhost__", "live", "wstest");
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
        let is_key = i == 1; // first sample is key frame to anchor the GOP
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

    // Connect and perform WebSocket upgrade.
    let mut stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("connect to server");

    let ws_key = "dGhlIHNhbXBsZSBub25jZQ==";
    let upgrade_req = format!(
        "GET /live/wstest.flv HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        TEST_PORT, ws_key
    );

    let resp = http_raw(&mut stream, upgrade_req.as_bytes()).await;
    let resp_str = String::from_utf8_lossy(&resp);

    assert!(
        resp_str.contains("101"),
        "expected 101, got: {}",
        resp_str.lines().next().unwrap_or("")
    );
    let expected_accept = compute_accept_key(ws_key);
    assert!(
        resp_str.contains(&expected_accept),
        "expected Sec-WebSocket-Accept header with key {}",
        expected_accept
    );

    // --- Read WebSocket frames ---
    // Expect: FLV header + metadata + config + 5 samples + 1 live publish = 9 frames.
    let mut frames_received = 0u32;

    let result = timeout(Duration::from_secs(5), async {
        let mut read_buf = BytesMut::with_capacity(65536);

        loop {
            if let Some((frame, consumed)) = WsFrame::decode(&read_buf[..]) {
                read_buf.advance(consumed);
                frames_received += 1;

                if frames_received == 1 {
                    assert_eq!(&frame.payload[..3], b"FLV");
                }

                // After cached frames are sent, publish a live frame.
                if frames_received == 7 {
                    source
                        .publish(MediaFrame::new_video(
                            0,
                            CodecId::H264,
                            240,
                            240,
                            240,
                            avcc_sample(false, 99),
                            false,
                        ))
                        .unwrap();
                }

                if frames_received >= 9 {
                    break;
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
        frames_received
    })
    .await;

    let count = result.expect("timeout waiting for WS frames");
    assert!(
        count >= 9,
        "expected at least 9 frames (header + metadata + 6 cached + 1 live), got {}",
        count
    );

    // Clean shutdown: send WebSocket close frame.
    let close = WsFrame::close(1000, "bye");
    let _ = stream.write_all(&close.encode_server()).await;

    source.close();
}
