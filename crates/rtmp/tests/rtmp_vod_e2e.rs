use bytes::Bytes;
use rand::RngExt;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, PayloadFormat};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_mp4::{Mp4Muxer, Mp4VodLibrary};
use zlmediakit_rtmp::amf::{AmfDecoder, AmfEncoder, AmfValue};
use zlmediakit_rtmp::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use zlmediakit_rtmp::RtmpServer;

fn video_config() -> MediaFrame {
    let mut frame = MediaFrame::new_video(
        1,
        CodecId::H264,
        0,
        0,
        0,
        Bytes::from_static(&[
            0x17, 0, 0, 0, 0, 1, 0x42, 0, 0x1f, 0xff, 0xe1, 0, 10, 0x67, 0x42, 0, 0x1f, 0x9a, 0x66,
            2, 0x80, 0x2c, 0x8e, 1, 0, 4, 0x68, 0xee, 0x3c, 0x80,
        ]),
        true,
    )
    .with_payload_format(PayloadFormat::Flv);
    frame.config_frame = true;
    frame
}

fn video_frame(timestamp: u32, key: bool, marker: u8) -> MediaFrame {
    let nalu_header = if key { 0x65 } else { 0x41 };
    MediaFrame::new_video(
        1,
        CodecId::H264,
        timestamp,
        u64::from(timestamp),
        u64::from(timestamp),
        Bytes::from(vec![
            if key { 0x17 } else { 0x27 },
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            2,
            nalu_header,
            marker,
        ]),
        key,
    )
    .with_payload_format(PayloadFormat::Flv)
}

fn sample_mp4() -> Bytes {
    let mut muxer = Mp4Muxer::new();
    muxer.push_frame(&video_config());
    for frame in [
        video_frame(0, true, 0x10),
        video_frame(100, false, 0x11),
        video_frame(200, true, 0x20),
        video_frame(300, false, 0x21),
    ] {
        muxer.push_frame(&frame);
    }
    muxer.finalize().freeze()
}

async fn handshake(stream: &mut TcpStream) {
    let mut c1 = vec![0u8; 1536];
    rand::rng().fill(&mut c1[8..]);
    let mut c0c1 = vec![3];
    c0c1.extend_from_slice(&c1);
    stream.write_all(&c0c1).await.unwrap();
    let mut response = vec![0; 3073];
    stream.read_exact(&mut response).await.unwrap();
    assert_eq!(response[0], 3);
    stream.write_all(&response[1..1537]).await.unwrap();
}

async fn send_command(stream: &mut TcpStream, encoder: &RtmpMessageEncoder, values: &[AmfValue]) {
    let message = RtmpMessage::Amf0Command {
        stream_id: if values
            .first()
            .is_some_and(|value| matches!(value, AmfValue::String(command) if command == "play"))
        {
            1
        } else {
            0
        },
        timestamp: 0,
        data: AmfEncoder::encode(values).freeze(),
    };
    stream.write_all(&encoder.encode(&message)).await.unwrap();
}

async fn read_available(
    stream: &mut TcpStream,
    parser: &mut RtmpMessageParser,
) -> Vec<RtmpMessage> {
    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut buffer = [0u8; 65_536];
    let mut messages = Vec::new();
    loop {
        match stream.try_read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => messages.extend(parser.feed(&buffer[..size])),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    messages
}

#[tokio::test]
async fn rtmp_mp4_vod_seeks_to_preceding_keyframe_and_stops() {
    let root = tempdir().unwrap();
    let archive = root.path().join("live/camera");
    tokio::fs::create_dir_all(&archive).await.unwrap();
    tokio::fs::write(archive.join("sample.mp4"), sample_mp4())
        .await
        .unwrap();
    let library = Arc::new(Mp4VodLibrary::new(root.path()));
    let direct = library
        .open("record", "live/camera/sample.mp4")
        .await
        .unwrap()
        .unwrap()
        .playback(250);
    assert_eq!(direct.start_ms, 200);
    assert_eq!(
        direct
            .frames
            .iter()
            .filter(|frame| !frame.config_frame)
            .map(|frame| frame.timestamp)
            .collect::<Vec<_>>(),
        vec![0, 100]
    );

    let manager = Arc::new(MediaSourceManager::new(None));
    let server = RtmpServer::new(
        "127.0.0.1:0",
        manager,
        Arc::new(EventBus::new(16)),
        StreamAuth::new(false, String::new()),
        HookClient::empty(),
        None,
        None,
    )
    .await
    .unwrap()
    .with_vod_library(library);
    let port = server.local_addr().unwrap().port();
    let task = tokio::spawn(async move { server.run().await });

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(&mut stream).await;
    let encoder = RtmpMessageEncoder::new();
    let mut parser = RtmpMessageParser::new();

    send_command(
        &mut stream,
        &encoder,
        &[
            AmfValue::String("connect".to_string()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![
                ("app".to_string(), AmfValue::String("record".to_string())),
                (
                    "tcUrl".to_string(),
                    AmfValue::String(format!("rtmp://127.0.0.1:{port}/record")),
                ),
            ]),
        ],
    )
    .await;
    let connect = read_available(&mut stream, &mut parser).await;
    if let Some(size) = connect.iter().find_map(|message| match message {
        RtmpMessage::SetChunkSize(size) => Some(*size),
        _ => None,
    }) {
        parser.set_chunk_size(size);
    }

    send_command(
        &mut stream,
        &encoder,
        &[
            AmfValue::String("createStream".to_string()),
            AmfValue::Number(2.0),
            AmfValue::Null,
        ],
    )
    .await;
    let _ = read_available(&mut stream, &mut parser).await;

    send_command(
        &mut stream,
        &encoder,
        &[
            AmfValue::String("play".to_string()),
            AmfValue::Number(3.0),
            AmfValue::Null,
            AmfValue::String("live/camera/sample.mp4".to_string()),
            AmfValue::Number(250.0),
            AmfValue::Number(-1.0),
            AmfValue::Boolean(true),
        ],
    )
    .await;

    let messages = timeout(Duration::from_secs(3), async {
        let mut all = Vec::new();
        loop {
            let batch = read_available(&mut stream, &mut parser).await;
            let stopped = batch.iter().any(|message| match message {
                RtmpMessage::Amf0Command { data, .. } => AmfDecoder::decode(data)
                    .ok()
                    .is_some_and(|values| format!("{values:?}").contains("NetStream.Play.Stop")),
                _ => false,
            });
            all.extend(batch);
            if stopped {
                break all;
            }
        }
    })
    .await
    .expect("VOD should finish");

    let media = messages
        .iter()
        .filter_map(|message| match message {
            RtmpMessage::Video {
                timestamp, data, ..
            } if data.get(1) == Some(&1) => Some((*timestamp, data)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(media.len(), 2, "seek should omit the first GOP");
    assert_eq!(media[0].0, 0, "selected keyframe must be timestamp-rebased");
    assert_eq!(
        media[0].1[0] >> 4,
        1,
        "first media frame must be a keyframe"
    );
    assert_eq!(media[0].1.last(), Some(&0x20));
    assert!(messages
        .iter()
        .any(|message| matches!(message, RtmpMessage::UserControl(1, _))));

    task.abort();
}
