use bytes::Bytes;
use rand::RngExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtmp::amf::{AmfDecoder, AmfEncoder, AmfValue};
use zlmediakit_rtmp::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use zlmediakit_rtmp::pull_client;
use zlmediakit_rtmp::RtmpServer;

const SRC_PORT: u16 = 19160;

fn build_c1() -> Vec<u8> {
    let mut c1 = vec![0u8; 1536];
    c1[..4].copy_from_slice(&0u32.to_be_bytes());
    c1[4..8].copy_from_slice(&0x00090000u32.to_be_bytes());
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
    stream.write_all(&s0s1[1..]).await.unwrap();
}

fn encode_video(stream_id: u32, timestamp: u32, data: Bytes) -> Vec<u8> {
    RtmpMessageEncoder::new()
        .encode(&RtmpMessage::Video {
            stream_id,
            timestamp,
            data,
        })
        .to_vec()
}

async fn send_and_recv(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
    data: &[u8],
) -> Vec<RtmpMessage> {
    stream.write_all(data).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
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

fn avcc_config() -> Bytes {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x1f, 0xff, 0xe0 | 1, 0x00, 0x0a,
    ];
    v.extend_from_slice(&sps);
    v.push(0x01);
    v.extend_from_slice(&[0x00, 0x04]);
    v.extend_from_slice(&pps);
    Bytes::from(v)
}

fn avcc_sample(key: bool) -> Bytes {
    let nalu = if key { [0x65u8, 0x9a, 0x00, 0x15, 0x20] } else { [0x41u8, 0x9a, 0x00, 0x10, 0x20] };
    let frame_type: u8 = if key { 0x10 } else { 0x20 };
    let mut v = vec![frame_type | 0x07, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05];
    v.extend_from_slice(&nalu);
    Bytes::from(v)
}

#[tokio::test]
async fn rtmp_pull_from_another_server() {
    let mgr_src = Arc::new(MediaSourceManager::new(None));
    let mgr_dst = Arc::new(MediaSourceManager::new(None));
    let auth = StreamAuth::new(false, String::new());

    let srv_src = RtmpServer::new(
        &format!("127.0.0.1:{}", SRC_PORT),
        mgr_src.clone(),
        Arc::new(EventBus::new(1024)),
        auth,
        HookClient::empty(),
        None,
        None,
    )
    .await
    .expect("source server start");
    tokio::spawn(async move { let _ = srv_src.run().await; });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // =========== Publish "pullsrc" to source server ===========
    let mut stream = TcpStream::connect(("127.0.0.1", SRC_PORT)).await.unwrap();
    stream.set_nodelay(true).unwrap();
    rtmp_handshake(&mut stream).await;

    let mut parser = RtmpMessageParser::new();
    let mut encoder = RtmpMessageEncoder::new();

    let connect_payload = AmfEncoder::encode(&[
        AmfValue::String("connect".to_string()),
        AmfValue::Number(1.0),
        AmfValue::Object(vec![
            ("app".to_string(), AmfValue::String("live".to_string())),
            ("tcUrl".to_string(), AmfValue::String(format!("rtmp://127.0.0.1:{}/live", SRC_PORT))),
        ]),
    ])
    .freeze();
    let connect_msg = RtmpMessage::Amf0Command { stream_id: 0, timestamp: 0, data: connect_payload };
    let msgs = send_and_recv(&mut stream, &mut parser, &encoder.encode(&connect_msg)).await;
    assert!(msgs.iter().any(|m| matches!(m, RtmpMessage::SetChunkSize(_))));
    encoder.set_chunk_size(4096);
    parser.set_chunk_size(4096);

    let cs_payload = AmfEncoder::encode(&[
        AmfValue::String("createStream".to_string()),
        AmfValue::Number(2.0),
        AmfValue::Null,
    ])
    .freeze();
    let cs_msg = RtmpMessage::Amf0Command { stream_id: 0, timestamp: 0, data: cs_payload };
    let msgs = send_and_recv(&mut stream, &mut parser, &encoder.encode(&cs_msg)).await;
    let stream_id = msgs.iter().find_map(|m| {
        if let RtmpMessage::Amf0Command { data, .. } = m {
            let vals = AmfDecoder::decode(data).ok()?;
            if matches!(vals.first(), Some(AmfValue::String(s)) if s == "_result") {
                vals.get(3).and_then(|v| match v { AmfValue::Number(n) => Some(*n as u32), _ => None })
            } else { None }
        } else { None }
    }).expect("_result for createStream");
    drop(msgs);

    let pub_payload = AmfEncoder::encode(&[
        AmfValue::String("publish".to_string()),
        AmfValue::Number(3.0),
        AmfValue::Null,
        AmfValue::String("pullsrc".to_string()),
        AmfValue::String("live".to_string()),
    ])
    .freeze();
    let pub_msg = RtmpMessage::Amf0Command { stream_id, timestamp: 0, data: pub_payload };
    let msgs = send_and_recv(&mut stream, &mut parser, &encoder.encode(&pub_msg)).await;
    assert!(msgs.iter().any(|m| matches!(m, RtmpMessage::Amf0Command { data, .. } if AmfDecoder::decode(data).ok().map_or(false, |v|
        matches!(v.first(), Some(AmfValue::String(s)) if s == "onStatus")
    ))));
    drop(msgs);

    // Send config + key frame
    stream.write_all(&encode_video(stream_id, 0, avcc_config())).await.unwrap();
    stream.write_all(&encode_video(stream_id, 0, avcc_sample(true))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // =========== Pull from source server into dst manager ===========
    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let pull_url = format!("rtmp://127.0.0.1:{}/live/pullsrc", SRC_PORT);
    let pull_handle = tokio::spawn({
        let mgr_dst = mgr_dst.clone();
        let stop = stop.clone();
        let stopped = stopped.clone();
        let url = pull_url.clone();
        async move {
            pull_client::start(&url, "__defaultVhost__", "live", "pulled", mgr_dst, stop, stopped).await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // =========== Verify pulled stream in dst manager ===========
    let source = mgr_dst.get("__defaultVhost__", "live", "pulled");
    assert!(source.is_some(), "pulled stream should exist in dst manager");
    let source = source.unwrap();

    let info = source.info.read().await;
    assert!(!info.tracks.is_empty(), "pulled stream should have tracks");
    let has_video = info.tracks.iter().any(|t| matches!(t, zlmediakit_core::media_frame::TrackInfo::Video(_)));
    assert!(has_video, "pulled stream should have video track");
    drop(info);

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(!frames.is_empty(), "pulled stream should have cached frames");
    drop(cached);

    // =========== Send more frames, verify pull client receives them ===========
    stream.write_all(&encode_video(stream_id, 100, avcc_sample(true))).await.unwrap();
    stream.write_all(&encode_video(stream_id, 200, avcc_sample(false))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let cached = source.gop_cache.read().await;
    let frames = cached.get_all_frames();
    assert!(frames.len() >= 3, "should have at least 3 frames after live push, got {}", frames.len());
    drop(cached);

    // Cleanup
    stop.notify_waiters();
    stopped.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(Duration::from_secs(1), pull_handle).await;

    if let Some(src) = mgr_src.get("__defaultVhost__", "live", "pullsrc") {
        src.close();
    }
}
