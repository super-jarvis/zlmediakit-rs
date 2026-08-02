use bytes::Bytes;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, PayloadFormat};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_mp4::{Mp4Muxer, Mp4VodLibrary};
use zlmediakit_rtsp::RtspServer;

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
            if key { 0x65 } else { 0x41 },
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

async fn request(stream: &mut TcpStream, pending: &mut Vec<u8>, request: String) -> String {
    stream.write_all(request.as_bytes()).await.unwrap();
    loop {
        if let Some(header_end) = pending.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let header_end = header_end + 4;
            let header = String::from_utf8_lossy(&pending[..header_end]);
            let body_length = header
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if pending.len() >= header_end + body_length {
                let bytes = pending
                    .drain(..header_end + body_length)
                    .collect::<Vec<_>>();
                return String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        let mut buffer = [0u8; 4096];
        let size = stream.read(&mut buffer).await.unwrap();
        assert!(size > 0, "RTSP server closed before responding");
        pending.extend_from_slice(&buffer[..size]);
    }
}

async fn next_interleaved(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Vec<u8> {
    loop {
        if pending.len() >= 4 && pending[0] == b'$' {
            let length = u16::from_be_bytes([pending[2], pending[3]]) as usize;
            if pending.len() >= length + 4 {
                return pending.drain(..length + 4).skip(4).collect();
            }
        }
        let mut buffer = [0u8; 4096];
        let size = stream.read(&mut buffer).await.unwrap();
        assert!(size > 0, "RTSP VOD stream closed before RTP arrived");
        pending.extend_from_slice(&buffer[..size]);
    }
}

#[tokio::test]
async fn rtsp_mp4_vod_honors_npt_seek_and_sends_selected_gop() {
    let root = tempdir().unwrap();
    let archive = root.path().join("live/camera");
    tokio::fs::create_dir_all(&archive).await.unwrap();
    tokio::fs::write(archive.join("sample.mp4"), sample_mp4())
        .await
        .unwrap();

    let server = RtspServer::new(
        "127.0.0.1:0",
        Arc::new(MediaSourceManager::new(None)),
        Arc::new(EventBus::new(16)),
        StreamAuth::new(false, String::new()),
        None,
        None,
        None,
    )
    .await
    .unwrap()
    .with_vod_library(Arc::new(Mp4VodLibrary::new(root.path())));
    let port = server.local_addr().unwrap().port();
    let task = tokio::spawn(async move { server.run().await });

    let url = format!("rtsp://127.0.0.1:{port}/record/live/camera/sample.mp4");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut pending = Vec::new();
    let describe = request(
        &mut stream,
        &mut pending,
        format!("DESCRIBE {url} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    assert!(describe.starts_with("RTSP/1.0 200"));
    assert!(describe.contains("m=video 0 RTP/AVP 96"));

    let setup = request(
        &mut stream,
        &mut pending,
        format!(
            "SETUP {url}/track1 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
    .await;
    assert!(setup.starts_with("RTSP/1.0 200"));
    let session = setup
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("session"))
        .map(|(_, value)| value.trim())
        .unwrap();

    let play = request(
        &mut stream,
        &mut pending,
        format!(
            "PLAY {url} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\nRange: npt=0.250-\r\n\r\n"
        ),
    )
    .await;
    assert!(play.starts_with("RTSP/1.0 200"));
    assert!(
        play.lines()
            .any(|line| line.eq_ignore_ascii_case("Range: npt=0.200-")),
        "server must report the actual preceding keyframe: {play}"
    );

    let marker = timeout(Duration::from_secs(2), async {
        loop {
            let packet = next_interleaved(&mut stream, &mut pending).await;
            if packet.len() > 13 && packet[12] & 0x1f == 5 {
                break packet[13];
            }
        }
    })
    .await
    .expect("selected IDR RTP packet should arrive");
    assert_eq!(marker, 0x20, "seek must omit the earlier GOP");

    task.abort();
}
