use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Duration;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_mp4::recorder::Mp4Recorder;

const TEST_BASE: &str = "/tmp/zlmediakit_mp4_test";

fn h264_config_data() -> Vec<u8> {
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
    v
}

#[tokio::test]
async fn mp4_recorder_writes_valid_file() {
    let dir_path = std::path::Path::new(TEST_BASE)
        .join("live")
        .join("teststream");
    let _ = std::fs::remove_dir_all(&dir_path);

    let mgr = Arc::new(MediaSourceManager::new());
    let source = mgr.get_or_create("__defaultVhost__", "live", "teststream");

    // Seed the cache with config + key frame
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, h264_config_data().into(), false);
    source.publish_and_cache(cfg).await;
    let key = MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        0,
        0,
        vec![
            0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x65, 0x9a, 0x00, 0x15, 0x20,
        ]
        .into(),
        true,
    );
    source.publish_and_cache(key).await;

    let stop = Arc::new(Notify::new());
    let rec_source = source.clone();
    let rec_stop = stop.clone();
    let rec_dir = TEST_BASE.to_string();
    let handle =
        tokio::spawn(async move { Mp4Recorder::record(rec_source, &rec_dir, rec_stop).await });

    // Publish live frames, then close the source.
    for i in 0..3 {
        let is_key = i == 2;
        let nalu_type: u8 = if is_key { 0x65 } else { 0x41 };
        let frame = MediaFrame::new_video(
            0,
            CodecId::H264,
            i * 100,
            (i * 100) as u64,
            (i * 100) as u64,
            vec![
                if is_key { 0x17 } else { 0x27 },
                0x01,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x05,
                nalu_type,
                0x9a,
                0x00,
                0x15,
                0x20,
            ]
            .into(),
            is_key,
        );
        source.publish_and_cache(frame).await;
    }

    source.close();

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("recorder should finish within 10s")
        .expect("recorder task should not panic");
    assert!(result.is_ok(), "recorder should succeed");

    // Verify output file.
    let entries = std::fs::read_dir(&dir_path).expect("recordings dir should exist");
    let mp4_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "mp4").unwrap_or(false))
        .collect();
    assert_eq!(mp4_files.len(), 1, "should have exactly one MP4 file");

    let mp4_path = &mp4_files[0].path();
    let data = std::fs::read(mp4_path).expect("read mp4 file");
    assert!(data.len() > 16, "file should be larger than minimal MP4");

    // Verify ftyp box
    assert_eq!(&data[4..8], b"ftyp", "should start with ftyp box");

    // Verify mdat box exists
    let mdat_pos = data.windows(4).position(|w| w == b"mdat");
    assert!(mdat_pos.is_some(), "should have mdat box");

    // Verify moov box exists
    let moov_pos = data.windows(4).position(|w| w == b"moov");
    assert!(moov_pos.is_some(), "should have moov box");

    let _ = std::fs::remove_file(mp4_path);
    let _ = std::fs::remove_dir(&dir_path);
}

#[tokio::test]
async fn mp4_recorder_no_frames_still_writes_header() {
    let dir_path = std::path::Path::new(TEST_BASE).join("empty").join("stream");
    let _ = std::fs::remove_dir_all(&dir_path);

    let mgr = Arc::new(MediaSourceManager::new());
    let source = mgr.get_or_create("__defaultVhost__", "empty", "stream");

    let stop = Arc::new(Notify::new());
    let rec_source = source.clone();
    let rec_stop = stop.clone();
    let rec_dir = TEST_BASE.to_string();
    let handle =
        tokio::spawn(async move { Mp4Recorder::record(rec_source, &rec_dir, rec_stop).await });

    source.close();

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("recorder should finish within 10s")
        .expect("recorder task should not panic");
    assert!(result.is_ok(), "recorder should succeed with no frames");

    // No frames means empty output - no file should be created.
    let has_files = std::fs::read_dir(&dir_path)
        .map(|entries| entries.filter_map(|e| e.ok()).count() > 0)
        .unwrap_or(false);
    assert!(!has_files, "no mp4 file should be created with zero frames");

    let _ = std::fs::remove_dir_all(dir_path);
}
