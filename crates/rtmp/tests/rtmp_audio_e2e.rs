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
use zlmediakit_rtmp::amf::{AmfDecoder, AmfEncoder, AmfValue};
use zlmediakit_rtmp::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use zlmediakit_rtmp::RtmpServer;

const TEST_PORT: u16 = 19156;

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

fn aac_config() -> Bytes {
    let mut v = vec![
        0xAF, // sound_format=10(AAC), sample_rate=3(44kHz), bits=1(16bit), channels=1(stereo)
        0x00, // AACPacketType=0 (config)
    ];
    // AudioSpecificConfig for AAC-LC, 44100Hz, stereo
    v.extend_from_slice(&[0x12, 0x10]);
    Bytes::from(v)
}

/// Build a minimal AAC-LC raw frame (dummy 2-byte data).
fn aac_frame() -> Bytes {
    let mut v = vec![
        0xAF, // sound_format=10(AAC), 44kHz, 16-bit, stereo
        0x01, // AACPacketType=1 (raw AAC frame)
    ];
    // Minimal AAC-LC frame data (just enough to be a valid frame)
    v.extend_from_slice(&[0x21, 0x00]);
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

fn encode_audio(stream_id: u32, timestamp: u32, data: Bytes) -> Vec<u8> {
    let encoder = RtmpMessageEncoder::new();
    let msg = RtmpMessage::Audio {
        stream_id,
        timestamp,
        data,
    };
    encoder.encode(&msg).to_vec()
}

async fn send_and_recv(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    data: &[u8],
) -> Vec<RtmpMessage> {
    stream.write_all(data).await.unwrap();
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

#[tokio::test]
async fn rtmp_audio_e2e_publish_audio_and_video() {
    let mgr = Arc::new(MediaSourceManager::new(None));
    let event_bus = Arc::new(EventBus::new(1024));
    let auth = StreamAuth::new(false, String::new());

    let addr = format!("127.0.0.1:{}", TEST_PORT);
    let srv = RtmpServer::new(&addr, mgr.clone(), event_bus, auth, HookClient::empty(), None, None)
        .await
        .expect("RtmpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect(("127.0.0.1", TEST_PORT))
        .await
        .expect("connect to RTMP server");
    stream.set_nodelay(true).unwrap();

    rtmp_handshake(&mut stream).await;

    let mut parser = RtmpMessageParser::new();
    let mut encoder = RtmpMessageEncoder::new();

    // connect
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

    encoder.set_chunk_size(4096);
    parser.set_chunk_size(4096);

    // createStream
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

    let stream_id = msgs
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
    assert_eq!(stream_id, 1);
    drop(msgs);

    // publish
    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String("audiotest".to_string()),
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
            AmfDecoder::decode(data).ok().map_or(false, |v| {
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
    drop(msgs);

    // --- Send frames: video config + audio config, then video + audio samples ---
    let config_video = avcc_config();
    stream
        .write_all(&encode_video(stream_id, 0, config_video))
        .await
        .unwrap();

    let config_audio = aac_config();
    stream
        .write_all(&encode_audio(stream_id, 0, config_audio))
        .await
        .unwrap();

    // Send video key frame
    let video_sample = avcc_sample(true);
    stream
        .write_all(&encode_video(stream_id, 40, video_sample))
        .await
        .unwrap();

    // Send audio frames
    for i in 0..3 {
        let audio_ts = 40 + i * 23;
        stream
            .write_all(&encode_audio(stream_id, audio_ts, aac_frame()))
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Verify via MediaSourceManager ---
    let source = mgr
        .get("__defaultVhost__", "live", "audiotest")
        .expect("source should exist after RTMP publish");
    let info = source.info.read().await;
    let has_video = info
        .tracks
        .iter()
        .any(|t| matches!(t, zlmediakit_core::media_frame::TrackInfo::Video(_)));
    let has_audio = info
        .tracks
        .iter()
        .any(|t| matches!(t, zlmediakit_core::media_frame::TrackInfo::Audio(_)));
    assert!(has_video, "should have video track");
    assert!(has_audio, "should have audio track");
    assert_eq!(info.tracks.len(), 2, "should have 2 tracks (audio+video)");
    drop(info);

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(
        frames.len() >= 4,
        "should have at least 4 cached frames (video config + audio config + video sample + audio samples), got {}",
        frames.len()
    );

    // Find audio frames
    let audio_frames: Vec<_> = frames
        .iter()
        .filter(|f| f.codec == zlmediakit_core::media_frame::CodecId::AAC)
        .collect();
    assert!(
        audio_frames.len() >= 2,
        "should have at least 2 audio frames (config + samples), got {}",
        audio_frames.len()
    );
    drop(cached);

    source.close();
}
