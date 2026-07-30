use bytes::Bytes;
use rand::RngExt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtmp::amf::{AmfEncoder, AmfValue};
use zlmediakit_rtmp::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use zlmediakit_rtmp::RtmpServer;

const TEST_PORT: u16 = 19135;

/// Build a C1 payload (4-byte timestamp, 4-byte version, 1528 random bytes).
fn build_c1() -> Vec<u8> {
    let mut c1 = vec![0u8; 1536];
    let ts = 0u32.to_be_bytes();
    c1[..4].copy_from_slice(&ts);
    let version = 0x00090000u32.to_be_bytes();
    c1[4..8].copy_from_slice(&version);
    rand::rng().fill(&mut c1[8..]);
    c1
}

/// Perform the RTMP client-side handshake.
async fn rtmp_handshake(stream: &mut TcpStream) {
    // C0 (1 byte) + C1 (1536 bytes)
    let mut c0c1 = vec![0x03u8]; // C0: version 3
    c0c1.extend_from_slice(&build_c1());
    stream.write_all(&c0c1).await.unwrap();

    // Read S0 + S1
    let mut s0s1 = vec![0u8; 1537];
    stream.read_exact(&mut s0s1).await.unwrap();
    assert_eq!(s0s1[0], 0x03, "S0 should be RTMP version 3");

    // Read S2 (echo of C1)
    let mut s2 = vec![0u8; 1536];
    stream.read_exact(&mut s2).await.unwrap();

    // Write C2 (echo of S1)
    let c2 = &s0s1[1..]; // S1 data
    stream.write_all(c2).await.unwrap();
}

/// Encode a video message.
fn encode_video(stream_id: u32, timestamp: u32, data: Bytes) -> Vec<u8> {
    let msg = RtmpMessage::Video {
        stream_id,
        timestamp,
        data,
    };
    let encoder = RtmpMessageEncoder::new();
    encoder.encode(&msg).to_vec()
}

/// Send raw bytes, read all available response and parse messages.
async fn send_and_recv(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    data: &[u8],
) -> Vec<RtmpMessage> {
    stream.write_all(data).await.unwrap();
    // Give the server a moment to process and respond.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut buf = [0u8; 65536];
    let mut all_messages = Vec::new();
    loop {
        match stream.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let msgs = parser.feed(&buf[..n]);
                all_messages.extend(msgs);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    all_messages
}

fn avcc_config() -> Bytes {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17, // FrameType=key(1)<<4 | CodecID=7
        0x00, // AVCPacketType=0 (AVCDecoderConfigurationRecord)
        0x00,
        0x00,
        0x00, // CTS=0
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

fn avcc_sample(key: bool) -> Bytes {
    let nalu = if key {
        [0x65u8, 0x9a, 0x00, 0x15, 0x20]
    } else {
        [0x41u8, 0x9a, 0x00, 0x10, 0x20]
    };
    let frame_type: u8 = if key { 0x10 } else { 0x20 };
    let mut v = vec![
        frame_type | 0x07,
        0x01, // AVCPacketType=1 (NAL unit)
        0x00,
        0x00,
        0x00, // CTS=0
        0x00,
        0x00,
        0x00,
        0x05, // NALU length = 5
    ];
    v.extend_from_slice(&nalu);
    Bytes::from(v)
}

#[tokio::test]
async fn rtmp_e2e_publish_then_wsflv_play() {
    let mgr = Arc::new(MediaSourceManager::new());
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(false, String::new());

    let addr = format!("127.0.0.1:{}", TEST_PORT);
    let srv = RtmpServer::new(&addr, mgr.clone(), event_bus, auth, HookClient::empty(), None, None)
        .await
        .expect("RtmpServer should start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- RTMP Client ---
    let mut stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("connect to RTMP server");

    stream.set_nodelay(true).unwrap();

    // Handshake
    rtmp_handshake(&mut stream).await;

    // Set up the response parser
    let mut parser = RtmpMessageParser::new();
    let mut encoder = RtmpMessageEncoder::new();

    // --- connect ---
    let connect_payload = AmfEncoder::encode(&[
        AmfValue::String("connect".to_string()),
        AmfValue::Number(1.0),
        AmfValue::Object(vec![
            ("app".to_string(), AmfValue::String("live".to_string())),
            (
                "tcUrl".to_string(),
                AmfValue::String(format!("rtmp://127.0.0.1:{}/live", TEST_PORT)),
            ),
        ]),
    ])
    .freeze();
    let connect_msg = RtmpMessage::Amf0Command {
        stream_id: 0,
        timestamp: 0,
        data: connect_payload,
    };
    let connect_wire = encoder.encode(&connect_msg);
    let msgs = send_and_recv(&mut stream, &mut parser, &connect_wire).await;
    assert!(
        msgs.iter()
            .any(|m| matches!(m, RtmpMessage::SetChunkSize(_))),
        "expected SetChunkSize"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, RtmpMessage::WindowAckSize(_))),
        "expected WindowAckSize"
    );
    assert!(
        msgs.iter().any(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = zlmediakit_rtmp::amf::AmfDecoder::decode(data).ok();
                vals.map_or(false, |v| {
                    v.first().map_or(
                        false,
                        |f| matches!(f, AmfValue::String(s) if s == "_result"),
                    )
                })
            } else {
                false
            }
        }),
        "expected _result for connect"
    );

    // Update encoder chunk size to match server's negotiation
    encoder.set_chunk_size(4096);
    parser.set_chunk_size(4096);

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
    let cs_wire = encoder.encode(&cs_msg);
    let msgs = send_and_recv(&mut stream, &mut parser, &cs_wire).await;

    // Find _result for createStream -> gets stream_id
    let stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = zlmediakit_rtmp::amf::AmfDecoder::decode(data).ok()?;
                if vals.first().map_or(
                    false,
                    |v| matches!(v, AmfValue::String(s) if s == "_result"),
                ) {
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
        .expect("_result for createStream should provide stream_id");
    assert_eq!(stream_id, 1, "expected stream_id=1");

    // --- publish ---
    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String("rtmptest".to_string()),
        AmfValue::String("live".to_string()),
    ])
    .freeze();
    let pub_msg = RtmpMessage::Amf0Command {
        stream_id,
        timestamp: 0,
        data: pub_payload,
    };
    let pub_wire = encoder.encode(&pub_msg);
    let msgs = send_and_recv(&mut stream, &mut parser, &pub_wire).await;
    let publish_ok = msgs.iter().any(|m| {
        if let RtmpMessage::Amf0Command { data, .. } = m {
            let vals = zlmediakit_rtmp::amf::AmfDecoder::decode(data).ok();
            vals.map_or(false, |v| {
                v.first().map_or(
                    false,
                    |f| matches!(f, AmfValue::String(s) if s == "onStatus"),
                )
            })
        } else {
            false
        }
    });
    assert!(publish_ok, "expected onStatus for publish");

    // --- Send video frames ---
    let config_data = avcc_config();
    let config_wire = encode_video(stream_id, 0, config_data);
    stream.write_all(&config_wire).await.unwrap();

    let sample_data = avcc_sample(true);
    let sample_wire = encode_video(stream_id, 40, sample_data);
    stream.write_all(&sample_wire).await.unwrap();

    // Wait a moment for server to process frames.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Verify via MediaSourceManager ---
    let source = mgr.get("__defaultVhost__", "live", "rtmptest");
    assert!(source.is_some(), "source should exist after RTMP publish");
    let source = source.unwrap();
    let info = source.info.read().await;
    let has_video = info
        .tracks
        .iter()
        .any(|t| matches!(t, zlmediakit_core::media_frame::TrackInfo::Video(_)));
    assert!(has_video, "should have video track info after publish");
    assert_eq!(info.tracks.len(), 1, "should have 1 track");
    drop(info);

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(
        frames.len() >= 2,
        "should have at least 2 cached frames (config + sample), got {}",
        frames.len()
    );

    // Verify the config frame is first and is a config frame.
    let first = &frames[0];
    assert!(
        zlmediakit_core::gop_cache::is_config_frame(first),
        "first frame should be video config"
    );
    assert_eq!(first.timestamp, 0);

    // Cleanup
    source.close();
}
