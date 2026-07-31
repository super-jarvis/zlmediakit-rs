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

const TEST_PORT: u16 = 19155;

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

fn build_hevc_config_record(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x01);
    if sps.len() >= 8 {
        out.push(sps[2]);
        out.extend_from_slice(&sps[3..7]);
        out.extend_from_slice(&[0u8; 6]);
        out.push(sps[7]);
    } else {
        out.push(0x20);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&[0u8; 6]);
        out.push(0x5D);
    }
    out.extend_from_slice(&[0, 0]);
    out.push(0);
    out.push(1);
    out.push(0);
    out.push(0);
    out.extend_from_slice(&[0, 0]);
    out.push(0x0F);
    out.push(0x03);
    for (nal_type, nalu) in [(32u8, vps), (33, sps), (34, pps)] {
        out.push(nal_type);
        out.extend_from_slice(&[0, 1]);
        out.extend_from_slice(&(nalu.len() as u16).to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

fn hevc_config_frame(vps: &[u8], sps: &[u8], pps: &[u8]) -> Bytes {
    let mut v = vec![
        0x1C, // key_frame(1)<<4 | codec_id(12) = 0x1C
        0x00, // AVCPacketType=0 (HEVCDecoderConfigurationRecord)
        0x00, 0x00, 0x00, // CTS = 0
    ];
    v.extend_from_slice(&build_hevc_config_record(vps, sps, pps));
    Bytes::from(v)
}

fn hevc_sample_frame(key: bool) -> Bytes {
    let nalu_type_byte: u8 = if key { 0x26 } else { 0x02 }; // type 19 (IDR) or type 1 (P-frame)
    let nalu = [nalu_type_byte, 0x01, 0x02, 0x03, 0x04];
    let frame_tag: u8 = if key { 0x1C } else { 0x2C };
    let mut v = vec![
        frame_tag, 0x01, // AVCPacketType=1 (NAL unit)
        0x00, 0x00, 0x00, // CTS = 0
    ];
    v.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
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

/// Send raw bytes, read all available response and parse messages.
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
async fn rtmp_hevc_e2e_publish_receives_media() {
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

    // Handshake
    rtmp_handshake(&mut stream).await;

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

    // --- publish ---
    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String("hevctest".to_string()),
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

    // --- Send HEVC frames ---
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

    let config_data = hevc_config_frame(&vps, &sps, &pps);
    let config_wire = encode_video(stream_id, 0, config_data);
    stream.write_all(&config_wire).await.unwrap();

    let sample_data = hevc_sample_frame(true);
    let sample_wire = encode_video(stream_id, 40, sample_data);
    stream.write_all(&sample_wire).await.unwrap();

    // Wait a moment for server to process frames.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Verify via MediaSourceManager ---
    let source = mgr.get("__defaultVhost__", "live", "hevctest");
    assert!(
        source.is_some(),
        "source should exist after RTMP HEVC publish"
    );
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
    assert_eq!(first.timestamp, 0);
    drop(cached);

    source.close();
}
