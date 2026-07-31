//! Integration tests for VideoTranscoder — uses real ffmpeg.
//!
//! Generates synthetic H.264/H.265 test streams via ffmpeg and verifies
//! that VideoTranscoder can re-encode them.

use zlmediakit_transcode::{parse_annex_b_frames, TranscodeConfig, VideoTranscoder};

/// Generates a short H.264 Annex-B test stream using ffmpeg.
async fn generate_h264_test_stream() -> Vec<u8> {
    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=320x240:rate=10",
            "-frames:v",
            "5",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .expect("ffmpeg generate test stream");
    assert!(!output.stdout.is_empty(), "ffmpeg should produce output");
    output.stdout
}

/// Generates a short H.265 Annex-B test stream using ffmpeg.
async fn generate_h265_test_stream() -> Vec<u8> {
    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=320x240:rate=10",
            "-frames:v",
            "5",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hevc",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .expect("ffmpeg generate test stream");
    assert!(!output.stdout.is_empty());
    output.stdout
}

#[tokio::test]
async fn transcode_h264_to_h264_passthrough() {
    let test_stream = generate_h264_test_stream().await;
    assert!(!test_stream.is_empty());
    assert!(
        test_stream.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
        "input should have start codes"
    );

    let config = TranscodeConfig {
        input_codec: "h264".into(),
        output_codec: "h264".into(),
        width: None,
        height: None,
        bitrate: "1M".into(),
        preset: "ultrafast".into(),
        name: "_out".into(),
        dst_app: None,
        dst_stream: None,
        enable_audio: false,
        ffmpeg_bin: "ffmpeg".into(),
    };

    let transcoder = VideoTranscoder::new(&config)
        .await
        .expect("create transcoder");

    let frames = transcoder
        .transcode_all(&test_stream)
        .await
        .expect("transcode");

    assert!(
        !frames.is_empty(),
        "should produce at least one output frame, got {}",
        frames.len()
    );

    // Verify output contains valid H.264 NAL units
    for frame in &frames {
        assert!(
            frame.data.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
            "output frame should have start codes"
        );
    }

    let has_key = frames.iter().any(|f| f.key_frame);
    assert!(has_key, "should have at least one keyframe");
}

#[tokio::test]
async fn transcode_h264_to_h264_with_scale() {
    let test_stream = generate_h264_test_stream().await;

    let config = TranscodeConfig {
        input_codec: "h264".into(),
        output_codec: "h264".into(),
        width: Some(160),
        height: Some(120),
        bitrate: "500k".into(),
        preset: "ultrafast".into(),
        name: "_out".into(),
        dst_app: None,
        dst_stream: None,
        enable_audio: false,
        ffmpeg_bin: "ffmpeg".into(),
    };

    let transcoder = VideoTranscoder::new(&config)
        .await
        .expect("create transcoder");

    let frames = transcoder
        .transcode_all(&test_stream)
        .await
        .expect("transcode");

    assert!(!frames.is_empty(), "should produce scaled output");

    for frame in &frames {
        assert!(
            frame.data.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
            "output should have start codes"
        );
    }
}

#[tokio::test]
async fn transcode_h264_to_h265() {
    let test_stream = generate_h264_test_stream().await;

    let config = TranscodeConfig {
        input_codec: "h264".into(),
        output_codec: "h265".into(),
        width: None,
        height: None,
        bitrate: "1M".into(),
        preset: "ultrafast".into(),
        name: "_out".into(),
        dst_app: None,
        dst_stream: None,
        enable_audio: false,
        ffmpeg_bin: "ffmpeg".into(),
    };

    let transcoder = VideoTranscoder::new(&config)
        .await
        .expect("create transcoder");

    let frames = transcoder
        .transcode_all(&test_stream)
        .await
        .expect("transcode");

    assert!(
        !frames.is_empty(),
        "should produce H.265 output, got {} frames",
        frames.len()
    );

    for frame in &frames {
        assert!(
            frame.data.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
            "output should have start codes"
        );
    }
}

#[tokio::test]
async fn transcode_h265_to_h264() {
    let test_stream = generate_h265_test_stream().await;

    let config = TranscodeConfig {
        input_codec: "h265".into(),
        output_codec: "h264".into(),
        width: Some(160),
        height: Some(120),
        bitrate: "800k".into(),
        preset: "ultrafast".into(),
        name: "_out".into(),
        dst_app: None,
        dst_stream: None,
        enable_audio: false,
        ffmpeg_bin: "ffmpeg".into(),
    };

    let transcoder = VideoTranscoder::new(&config)
        .await
        .expect("create transcoder");

    let frames = transcoder
        .transcode_all(&test_stream)
        .await
        .expect("transcode");

    assert!(!frames.is_empty(), "should convert H.265 -> H.264");

    for frame in &frames {
        assert!(
            frame.data.windows(3).any(|w| w == [0x00, 0x00, 0x01]),
            "output should have start codes"
        );
    }
}

#[test]
fn flv_to_annex_b_with_real_config() {
    let avcc_config = [
        0x01, 0x64, 0x00, 0x0a, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x0a, 0x01, 0x00, 0x04,
        0x68, 0xee, 0x3c, 0x80,
    ];

    let mut flv_frame = vec![0x17, 0x00, 0x00, 0x00, 0x00];
    flv_frame.extend_from_slice(&avcc_config);

    let annex_b = zlmediakit_transcode::config_frame_to_annex_b(&flv_frame);
    assert!(!annex_b.is_empty());
    assert!(
        annex_b.windows(4).any(|w| w == [0x00, 0x00, 0x00, 0x01]),
        "output should have start codes"
    );
}

#[test]
fn parse_annex_b_frames_smoke() {
    // Create a simple multi-frame Annex-B stream
    let frame1 = [0x00, 0x00, 0x00, 0x01, 0x25, 0x01, 0x02, 0x03];
    let frame2 = [0x00, 0x00, 0x00, 0x01, 0x21, 0x04, 0x05, 0x06];
    let mut stream = Vec::new();
    stream.extend_from_slice(&frame1);
    stream.extend_from_slice(&frame2);

    let frames = parse_annex_b_frames(&stream, "h264");
    assert_eq!(frames.len(), 2);
    assert!(frames[0].key_frame); // type=5 → IDR
    assert!(!frames[1].key_frame); // type=1 → non-IDR
}
