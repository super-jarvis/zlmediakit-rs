use bytes::Bytes;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use zlmediakit_core::media_frame::{
    AudioInfo, CodecId, MediaFrame, PayloadFormat, TrackInfo, VideoInfo,
};
use zlmediakit_core::MediaSourceManager;
use zlmediakit_srt::{push_client, SrtServer, SrtServerConfig};

fn reserve_udp_port() -> u16 {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn srt_caller_pushes_mpeg_ts_to_listener() {
    let port = reserve_udp_port();
    let remote_manager = Arc::new(MediaSourceManager::new(None));
    let server_stop = Arc::new(AtomicBool::new(false));
    let mut server = SrtServer::new(SrtServerConfig {
        addr: format!("127.0.0.1:{port}"),
        latency_ms: 40,
        passphrase: None,
        source_manager: remote_manager.clone(),
    });
    let server_stop_task = server_stop.clone();
    let server_task = tokio::task::spawn_blocking(move || server.run_blocking(server_stop_task));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let local_manager = Arc::new(MediaSourceManager::new(None));
    let source = local_manager.get_or_create("__defaultVhost__", "live", "camera");
    {
        let mut info = source.info.write().await;
        info.tracks.push(TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: true,
        }));
        info.tracks.push(TrackInfo::Audio(AudioInfo {
            codec: CodecId::AAC,
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
        }));
    }
    let mut video_config = MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        0,
        0,
        Bytes::from_static(&[
            0x17, 0, 0, 0, 0, 0x01, 0x64, 0, 0x0a, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x0a, 1, 0, 4,
            0x68, 0xee, 0x3c, 0x80,
        ]),
        false,
    )
    .with_payload_format(PayloadFormat::Flv);
    video_config.config_frame = true;
    source.publish_and_cache(video_config).await;
    source
        .publish_and_cache(
            MediaFrame::new_video(
                0,
                CodecId::H264,
                40,
                40,
                40,
                Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3, 4]),
                true,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        )
        .await;
    let mut audio_config = MediaFrame::new_audio(
        1,
        CodecId::AAC,
        0,
        0,
        0,
        Bytes::from_static(&[0xaf, 0, 0x12, 0x10]),
    )
    .with_payload_format(PayloadFormat::Flv);
    audio_config.config_frame = true;
    source.publish_and_cache(audio_config).await;
    source
        .publish_and_cache(
            MediaFrame::new_audio(
                1,
                CodecId::AAC,
                40,
                40,
                40,
                Bytes::from_static(&[0xaf, 1, 0x11, 0x22, 0x33, 0x44]),
            )
            .with_payload_format(PayloadFormat::Flv),
        )
        .await;

    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let push_url = format!("srt://127.0.0.1:{port}/live/forwarded?latency=40");
    let push_manager = local_manager.clone();
    let push_stop = stop.clone();
    let push_stopped = stopped.clone();
    let mut push_task = tokio::spawn(async move {
        push_client::start(
            &push_url,
            "__defaultVhost__",
            "live",
            "camera",
            push_manager,
            push_stop,
            push_stopped,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(250)).await;
    if push_task.is_finished() {
        let result = (&mut push_task).await.unwrap();
        server_stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        panic!("SRT push task ended before media transfer: {result:?}");
    }
    source
        .publish_and_cache(
            MediaFrame::new_video(
                0,
                CodecId::H264,
                80,
                80,
                80,
                Bytes::from_static(&[0, 0, 0, 1, 0x41, 5, 6, 7]),
                false,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        )
        .await;
    source
        .publish_and_cache(
            MediaFrame::new_audio(
                1,
                CodecId::AAC,
                80,
                80,
                80,
                Bytes::from_static(&[0xaf, 1, 0x55, 0x66, 0x77]),
            )
            .with_payload_format(PayloadFormat::Flv),
        )
        .await;

    let remote_wait = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(remote) = remote_manager.get("__defaultVhost__", "live", "forwarded") {
                let frames = remote.get_latest_gop_frames().await;
                let has_video = frames
                    .iter()
                    .any(|frame| frame.codec == CodecId::H264 && frame.key_frame);
                let has_audio = frames
                    .iter()
                    .any(|frame| frame.codec == CodecId::AAC && !frame.config_frame);
                if has_video && has_audio {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    tokio::pin!(remote_wait);
    let transfer_error = tokio::select! {
        push_result = &mut push_task => Some(format!("SRT push task ended early: {:?}", push_result.unwrap())),
        remote_result = &mut remote_wait => remote_result.err().map(|error| format!("remote SRT listener did not receive H.264/AAC MPEG-TS frames: {error}")),
    };
    if let Some(error) = transfer_error {
        let mut diagnostics = Vec::new();
        for source in remote_manager.list() {
            let frames = source.get_latest_gop_frames().await;
            diagnostics.push((
                source.vhost.clone(),
                source.app.clone(),
                source.stream.clone(),
                frames
                    .iter()
                    .map(|frame| (frame.codec, frame.key_frame, frame.data.len()))
                    .collect::<Vec<_>>(),
            ));
        }
        server_stop.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        panic!("{error}; remote sources: {diagnostics:?}");
    }

    stopped.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(2), push_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server_stop.store(true, Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
