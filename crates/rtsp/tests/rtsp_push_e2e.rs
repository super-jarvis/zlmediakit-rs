use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::EventBus;
use zlmediakit_core::media_frame::{
    AudioInfo, CodecId, MediaFrame, PayloadFormat, TrackInfo, VideoInfo,
};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtsp::{push_client, RtspServer};

#[tokio::test]
async fn rtsp_push_client_records_interleaved_rtp_to_remote_server() {
    let remote_manager = Arc::new(MediaSourceManager::new(None));
    let remote_server = RtspServer::new(
        "127.0.0.1:0",
        remote_manager.clone(),
        Arc::new(EventBus::new(128)),
        StreamAuth::new_with_realm(
            true,
            "push-secret".to_string(),
            Some("push-realm".to_string()),
            HashMap::from([("publisher".to_string(), "push-secret".to_string())]),
        ),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let remote_port = remote_server.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move { remote_server.run().await });

    let local_manager = Arc::new(MediaSourceManager::new(None));
    let source = local_manager.get_or_create("__defaultVhost__", "live", "camera");
    let mut source_info = source.info.write().await;
    source_info.tracks.push(TrackInfo::Video(VideoInfo {
        codec: CodecId::H264,
        width: 1280,
        height: 720,
        fps: 25.0,
        key_frame: true,
    }));
    source_info.tracks.push(TrackInfo::Audio(AudioInfo {
        codec: CodecId::AAC,
        sample_rate: 44_100,
        channels: 2,
        bits_per_sample: 16,
    }));
    drop(source_info);
    let mut config = MediaFrame::new_video(
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
    config.config_frame = true;
    source.publish_and_cache(config).await;
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
    let push_stop = stop.clone();
    let push_stopped = stopped.clone();
    let push_manager = local_manager.clone();
    let push_task = tokio::spawn(async move {
        push_client::start(
            &format!("rtsp://publisher:push-secret@127.0.0.1:{remote_port}/live/forwarded"),
            "__defaultVhost__",
            "live",
            "camera",
            push_manager,
            push_stop,
            push_stopped,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(3), async {
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("remote RTSP server did not receive the pushed H.264/AAC frames");

    stopped.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(2), push_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server_task.abort();
}
