use bytes::Bytes;
use rand::RngExt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtmp::amf::{AmfDecoder, AmfEncoder, AmfValue};
use zlmediakit_rtmp::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use zlmediakit_rtmp::RtmpServer;

const TEST_PORT: u16 = 19148;

fn build_c1() -> Vec<u8> {
    let mut c1 = vec![0u8; 1536];
    let ts = 0u32.to_be_bytes();
    c1[..4].copy_from_slice(&ts);
    let version = 0x00090000u32.to_be_bytes();
    c1[4..8].copy_from_slice(&version);
    rand::rng().fill(&mut c1[8..]);
    c1
}

async fn rtmp_handshake(stream: &mut TcpStream) {
    let mut c0c1 = vec![0x03u8];
    c0c1.extend_from_slice(&build_c1());
    stream.write_all(&c0c1).await.unwrap();

    let mut s0s1 = vec![0u8; 1537];
    stream.read_exact(&mut s0s1).await.unwrap();
    assert_eq!(s0s1[0], 0x03);

    let mut s2 = vec![0u8; 1536];
    stream.read_exact(&mut s2).await.unwrap();

    let c2 = &s0s1[1..];
    stream.write_all(c2).await.unwrap();
}

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

fn avcc_sample(key: bool) -> Bytes {
    let nalu = if key {
        [0x65u8, 0x9a, 0x00, 0x15, 0x20]
    } else {
        [0x41u8, 0x9a, 0x00, 0x10, 0x20]
    };
    let frame_type: u8 = if key { 0x10 } else { 0x20 };
    let mut v = vec![
        frame_type | 0x07,
        0x01,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x05,
    ];
    v.extend_from_slice(&nalu);
    Bytes::from(v)
}

fn encode_video(stream_id: u32, timestamp: u32, data: Bytes) -> Vec<u8> {
    let encoder = RtmpMessageEncoder::new();
    let msg = RtmpMessage::Video {
        stream_id,
        timestamp,
        data,
    };
    encoder.encode(&msg).to_vec()
}

/// Read all available data from stream and parse messages.
async fn read_all(stream: &mut TcpStream, parser: &mut RtmpMessageParser) -> Vec<RtmpMessage> {
    let mut buf = [0u8; 65536];
    let mut all = Vec::new();
    loop {
        match stream.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend(parser.feed(&buf[..n])),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    all
}

/// Send data and wait briefly for responses.
async fn send_and_recv(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    data: &[u8],
) -> Vec<RtmpMessage> {
    stream.write_all(data).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    read_all(stream, parser).await
}

#[tokio::test]
async fn rtmp_play_receives_video() {
    let mgr = Arc::new(MediaSourceManager::new());
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(false, String::new());

    let srv = RtmpServer::new(
        &format!("127.0.0.1:{}", TEST_PORT),
        mgr.clone(),
        event_bus,
        auth,
        None,
        None,
    )
    .await
    .expect("RtmpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ================ Publisher ================
    let mut pub_stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("publisher connect");
    pub_stream.set_nodelay(true).unwrap();
    rtmp_handshake(&mut pub_stream).await;

    let mut pub_parser = RtmpMessageParser::new();
    let mut pub_encoder = RtmpMessageEncoder::new();

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

    let msgs = send_and_recv(
        &mut pub_stream,
        &mut pub_parser,
        &pub_encoder.encode(&connect_msg),
    )
    .await;
    assert!(msgs
        .iter()
        .any(|m| matches!(m, RtmpMessage::SetChunkSize(_))));
    assert!(msgs
        .iter()
        .any(|m| matches!(m, RtmpMessage::WindowAckSize(_))));
    assert!(msgs.iter().any(|m| {
        if let RtmpMessage::Amf0Command { data, .. } = m {
            AmfDecoder::decode(data).ok().map_or(false, |v| {
                v.first().map_or(
                    false,
                    |f| matches!(f, AmfValue::String(s) if s == "_result"),
                )
            })
        } else {
            false
        }
    }));

    pub_encoder.set_chunk_size(4096);
    pub_parser.set_chunk_size(4096);

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
    let msgs = send_and_recv(
        &mut pub_stream,
        &mut pub_parser,
        &pub_encoder.encode(&cs_msg),
    )
    .await;
    let pub_stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = AmfDecoder::decode(data).ok()?;
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
        .expect("_result for createStream");
    assert_eq!(pub_stream_id, 1);
    drop(msgs);

    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String("playtest".to_string()),
        AmfValue::String("live".to_string()),
    ])
    .freeze();
    let pub_msg = RtmpMessage::Amf0Command {
        stream_id: pub_stream_id,
        timestamp: 0,
        data: pub_payload,
    };
    let msgs = send_and_recv(
        &mut pub_stream,
        &mut pub_parser,
        &pub_encoder.encode(&pub_msg),
    )
    .await;
    assert!(msgs.iter().any(|m| {
        if let RtmpMessage::Amf0Command { data, .. } = m {
            AmfDecoder::decode(data).ok().map_or(false, |v| {
                v.first().map_or(
                    false,
                    |f| matches!(f, AmfValue::String(s) if s == "onStatus"),
                )
            })
        } else {
            false
        }
    }));
    drop(msgs);

    // Send config + key frame
    pub_stream
        .write_all(&encode_video(pub_stream_id, 0, avcc_config()))
        .await
        .unwrap();
    pub_stream
        .write_all(&encode_video(pub_stream_id, 0, avcc_sample(true)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ================ Player ================
    let mut play_stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("player connect");
    play_stream.set_nodelay(true).unwrap();
    rtmp_handshake(&mut play_stream).await;

    let mut play_parser = RtmpMessageParser::new();
    let play_encoder = RtmpMessageEncoder::new();

    // connect
    let msgs = send_and_recv(
        &mut play_stream,
        &mut play_parser,
        &play_encoder.encode(&connect_msg),
    )
    .await;
    assert!(msgs
        .iter()
        .any(|m| matches!(m, RtmpMessage::WindowAckSize(_))));
    let chunk_size = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::SetChunkSize(sz) = m {
                Some(*sz)
            } else {
                None
            }
        })
        .unwrap_or(128);
    play_parser.set_chunk_size(chunk_size);
    assert!(msgs.iter().any(|m| {
        if let RtmpMessage::Amf0Command { data, .. } = m {
            AmfDecoder::decode(data).ok().map_or(false, |v| {
                v.first().map_or(
                    false,
                    |f| matches!(f, AmfValue::String(s) if s == "_result"),
                )
            })
        } else {
            false
        }
    }));
    drop(msgs);

    // createStream
    let msgs = send_and_recv(
        &mut play_stream,
        &mut play_parser,
        &play_encoder.encode(&cs_msg),
    )
    .await;
    let play_stream_id = msgs
        .iter()
        .find_map(|m| {
            if let RtmpMessage::Amf0Command { data, .. } = m {
                let vals = AmfDecoder::decode(data).ok()?;
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
        .expect("_result for createStream");
    assert_eq!(play_stream_id, 1);
    drop(msgs);

    // play("playtest")
    let play_cmd_payload = AmfEncoder::encode(&[
        AmfValue::String("play".to_string()),
        AmfValue::Number(4.0),
        AmfValue::Null,
        AmfValue::String("playtest".to_string()),
    ])
    .freeze();
    let play_cmd_msg = RtmpMessage::Amf0Command {
        stream_id: play_stream_id,
        timestamp: 0,
        data: play_cmd_payload,
    };
    play_stream
        .write_all(&play_encoder.encode(&play_cmd_msg))
        .await
        .unwrap();

    // Wait for play response + cached frames
    tokio::time::sleep(Duration::from_millis(200)).await;
    let initial = read_all(&mut play_stream, &mut play_parser).await;
    let initial_video = initial
        .iter()
        .filter(|m| matches!(m, RtmpMessage::Video { .. }))
        .count();
    println!(
        "initial video frames: {} (total msgs: {})",
        initial_video,
        initial.len()
    );
    drop(initial);

    // Send more frames from publisher
    pub_stream
        .write_all(&encode_video(pub_stream_id, 100, avcc_sample(true)))
        .await
        .unwrap();
    pub_stream
        .write_all(&encode_video(pub_stream_id, 150, avcc_sample(false)))
        .await
        .unwrap();

    // Wait and read live frames
    tokio::time::sleep(Duration::from_millis(300)).await;
    let live = read_all(&mut play_stream, &mut play_parser).await;
    let live_video = live
        .iter()
        .filter(|m| matches!(m, RtmpMessage::Video { .. }))
        .count();
    println!(
        "live video frames: {} (total msgs: {})",
        live_video,
        live.len()
    );
    drop(live);

    // Try one more read after another short wait
    tokio::time::sleep(Duration::from_millis(300)).await;
    let more = read_all(&mut play_stream, &mut play_parser).await;
    let more_video = more
        .iter()
        .filter(|m| matches!(m, RtmpMessage::Video { .. }))
        .count();
    println!("more video frames: {}", more_video);
    drop(more);

    let total = initial_video + live_video + more_video;
    println!("total video frames: {}", total);
    assert!(
        total >= 2,
        "player should receive at least 2 video frames, got {}",
        total
    );

    // Cleanup
    if let Some(source) = mgr.get("__defaultVhost__", "live", "playtest") {
        source.close();
    }
}
