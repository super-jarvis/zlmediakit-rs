use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Duration;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_flv::FlvRecorder;

const TEST_DIR: &str = "/tmp/zlmediakit_flv_test";

fn h264_config_data() -> Vec<u8> {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x1f, 0xff, 0xe0 | 1, 0x00, 0x0a,
    ];
    v.extend_from_slice(&sps);
    v.push(0x01);
    v.extend_from_slice(&[0x00, 0x04]);
    v.extend_from_slice(&pps);
    v
}

#[tokio::test]
async fn flv_recorder_writes_valid_file() {
    let _ = std::fs::create_dir_all(TEST_DIR);
    let dir_path = std::path::Path::new(TEST_DIR).join("live").join("teststream");
    let _ = std::fs::remove_dir_all(&dir_path);

    let mgr = Arc::new(MediaSourceManager::new());
    let source = mgr.get_or_create("__defaultVhost__", "live", "teststream");

    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, h264_config_data().into(), false);
    source.publish_and_cache(cfg).await;
    let key = MediaFrame::new_video(
        0, CodecId::H264, 0, 0, 0,
        vec![0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x65, 0x9a, 0x00, 0x15, 0x20]
            .into(),
        true,
    );
    source.publish_and_cache(key).await;

    let stop = Arc::new(Notify::new());
    let rec_source = source.clone();
    let rec_stop = stop.clone();
    let rec_dir = TEST_DIR.to_string();
    let handle = tokio::spawn(async move {
        FlvRecorder::record(rec_source, &rec_dir, rec_stop).await
    });

    // Push frames, then signal stop. If the recorder hasn't entered its
    // select loop yet, source.close() will also terminate it via Closed.
    for i in 0..3 {
        let is_key = i == 2;
        let nalu_type: u8 = if is_key { 0x65 } else { 0x41 };
        let frame = MediaFrame::new_video(
            0, CodecId::H264, i * 100, (i * 100) as u64, (i * 100) as u64,
            vec![
                if is_key { 0x17 } else { 0x27 },
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05,
                nalu_type, 0x9a, 0x00, 0x15, 0x20,
            ]
            .into(),
            is_key,
        );
        source.publish_and_cache(frame).await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.notify_waiters();
    source.close();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("recorder should finish within 5s")
        .expect("recorder task should not panic");
    assert!(result.is_ok(), "recorder should succeed");

    // Verify output file
    let entries = std::fs::read_dir(&dir_path).expect("recordings dir should exist");
    let flv_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "flv").unwrap_or(false))
        .collect();
    assert_eq!(flv_files.len(), 1, "should have exactly one FLV file");

    let flv_path = &flv_files[0].path();
    let data = std::fs::read(flv_path).expect("read flv file");
    assert!(data.len() > 13, "file should be larger than FLV header");
    assert_eq!(&data[..3], b"FLV", "should start with FLV signature");
    assert_eq!(data[3], 0x01, "version 1");
    assert!(data[4] & 0x01 != 0, "should have video flag");

    let _ = std::fs::remove_file(flv_path);
    let _ = std::fs::remove_dir(&dir_path);
}

#[tokio::test]
async fn flv_recorder_no_frames_still_writes_header() {
    let _ = std::fs::create_dir_all(TEST_DIR);
    let dir_path = std::path::Path::new(TEST_DIR).join("empty").join("stream");
    let _ = std::fs::remove_dir_all(&dir_path);

    let mgr = Arc::new(MediaSourceManager::new());
    let source = mgr.get_or_create("__defaultVhost__", "empty", "stream");

    let stop = Arc::new(Notify::new());
    let rec_source = source.clone();
    let rec_stop = stop.clone();
    let rec_dir = TEST_DIR.to_string();
    let handle = tokio::spawn(async move {
        FlvRecorder::record(rec_source, &rec_dir, rec_stop).await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.notify_waiters();
    source.close();

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("recorder should finish within 5s")
        .expect("recorder task should not panic");
    assert!(result.is_ok(), "recorder should succeed with no frames");

    let data = std::fs::read(
        dir_path
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(!data.is_empty(), "should have at least FLV header");
    assert_eq!(&data[..3], b"FLV", "should start with FLV signature");

    std::fs::remove_dir_all(dir_path.parent().unwrap()).ok();
}
