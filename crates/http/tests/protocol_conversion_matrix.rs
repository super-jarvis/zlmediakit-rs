use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};
use zlmediakit_flv::{FlvDemuxer, FlvMuxer};
use zlmediakit_hls::HlsMuxer;
use zlmediakit_mp4::{Fmp4Muxer, Mp4Demuxer, Mp4Muxer};

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("failed to launch media tool");
    assert!(
        output.status.success(),
        "media tool failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_probe_and_decode(path: &Path, video_codec: &str) {
    for (selector, codec) in [("v:0", video_codec), ("a:0", "aac")] {
        let output = run(Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                selector,
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let codecs: Vec<_> = stdout.lines().filter(|line| !line.is_empty()).collect();
        assert!(!codecs.is_empty(), "ffprobe did not find {selector}");
        assert!(
            codecs.iter().all(|actual| *actual == codec),
            "expected {codec} for {selector}, got {codecs:?}"
        );
    }

    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"]));
}

fn generate_reference_flv(path: &Path) {
    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100",
            "-t",
            "1.2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "25",
            "-bf",
            "0",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ar",
            "44100",
            "-ac",
            "2",
            "-f",
            "flv",
            "-y",
        ])
        .arg(path));
}

fn generate_reference_hevc_mp4(path: &Path) {
    run(Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=600:sample_rate=44100",
            "-t",
            "1.2",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error:keyint=25:min-keyint=25:bframes=0",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ar",
            "44100",
            "-ac",
            "2",
            "-movflags",
            "+faststart",
            "-y",
        ])
        .arg(path));
}

fn preserve_artifact(path: &Path) {
    let Some(directory) = std::env::var_os("ZLMEDIAKIT_MATRIX_ARTIFACT_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    fs::create_dir_all(directory).expect("create artifact directory");
    fs::copy(path, directory.join(path.file_name().unwrap())).expect("preserve media artifact");
}

fn demux_flv(path: &Path) -> Vec<MediaFrame> {
    let input = fs::read(path).expect("read generated FLV");
    let mut demuxer = FlvDemuxer::new();
    demuxer.feed(&input);
    assert_eq!(demuxer.parse_header(), Some(true));

    let mut frames = Vec::new();
    while let Some(frame) = demuxer.parse_tag() {
        frames.push(frame);
    }
    assert!(frames.iter().any(|frame| {
        frame.frame_type == FrameType::Video && frame.codec == CodecId::H264 && frame.config_frame
    }));
    assert!(frames.iter().any(|frame| {
        frame.frame_type == FrameType::Audio && frame.codec == CodecId::AAC && frame.config_frame
    }));
    assert!(frames.iter().any(|frame| {
        frame.frame_type == FrameType::Video && !frame.config_frame && frame.key_frame
    }));
    frames
}

#[test]
fn ffmpeg_flv_ingress_converts_to_playable_flv_ts_mp4_and_fmp4() {
    if !tool_available("ffmpeg") || !tool_available("ffprobe") {
        eprintln!("skipping protocol conversion matrix: ffmpeg/ffprobe are unavailable");
        return;
    }

    let dir = tempdir().expect("create temporary media directory");
    let source_path = dir.path().join("source.flv");
    generate_reference_flv(&source_path);
    assert_probe_and_decode(&source_path, "h264");
    let frames = demux_flv(&source_path);

    let remuxed_flv_path = dir.path().join("remuxed.flv");
    let mut flv_muxer = FlvMuxer::new();
    let mut remuxed_flv = flv_muxer.write_header().to_vec();
    for frame in &frames {
        remuxed_flv.extend_from_slice(&flv_muxer.write_tag(frame).expect("write FLV tag"));
    }
    fs::write(&remuxed_flv_path, remuxed_flv).expect("write remuxed FLV");
    assert_probe_and_decode(&remuxed_flv_path, "h264");

    let ts_path = dir.path().join("segment.ts");
    let mut hls_muxer = HlsMuxer::new(2.0);
    for frame in frames.iter().cloned() {
        hls_muxer.push_frame(frame);
    }
    let (_, ts) = hls_muxer.flush().expect("flush HLS transport stream");
    fs::write(&ts_path, ts).expect("write transport stream");
    assert_probe_and_decode(&ts_path, "h264");

    let mp4_path = dir.path().join("recording.mp4");
    let mut mp4_muxer = Mp4Muxer::new();
    for frame in &frames {
        mp4_muxer.push_frame(frame);
    }
    fs::write(&mp4_path, mp4_muxer.finalize()).expect("write MP4");
    preserve_artifact(&mp4_path);
    assert_probe_and_decode(&mp4_path, "h264");

    let fmp4_path = dir.path().join("fragmented.mp4");
    let mut fmp4_muxer = Fmp4Muxer::new(90_000);
    for frame in &frames {
        fmp4_muxer.push_frame(frame);
    }
    let init = fmp4_muxer.init_segment();
    let media = fmp4_muxer
        .flush_segment()
        .expect("flush fMP4 media segment");
    let mut fragmented = init.data.to_vec();
    fragmented.extend_from_slice(&media.data);
    fs::write(&fmp4_path, fragmented).expect("write fragmented MP4");
    preserve_artifact(&fmp4_path);
    assert_probe_and_decode(&fmp4_path, "h264");
}

#[test]
fn ffmpeg_mp4_hevc_ingress_converts_to_playable_ts_mp4_and_fmp4() {
    if !tool_available("ffmpeg") || !tool_available("ffprobe") {
        eprintln!("skipping HEVC conversion matrix: ffmpeg/ffprobe are unavailable");
        return;
    }
    if !Command::new("ffmpeg")
        .args(["-hide_banner", "-h", "encoder=libx265"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping HEVC conversion matrix: libx265 is unavailable");
        return;
    }

    let dir = tempdir().expect("create temporary HEVC media directory");
    let source_path = dir.path().join("source-hevc.mp4");
    generate_reference_hevc_mp4(&source_path);
    assert_probe_and_decode(&source_path, "hevc");

    let source = fs::read(&source_path).expect("read generated HEVC MP4");
    let demuxer = Mp4Demuxer::from_bytes(&source).expect("demux generated HEVC MP4");
    let frames = demuxer.interleaved_media_frames();
    assert!(frames.iter().any(|frame| {
        frame.frame_type == FrameType::Video && frame.codec == CodecId::H265 && frame.config_frame
    }));
    assert!(frames.iter().any(|frame| {
        frame.frame_type == FrameType::Audio && frame.codec == CodecId::AAC && frame.config_frame
    }));

    let ts_path = dir.path().join("hevc-segment.ts");
    let mut hls_muxer = HlsMuxer::new(2.0);
    for frame in frames.iter().cloned() {
        hls_muxer.push_frame(frame);
    }
    let (_, ts) = hls_muxer.flush().expect("flush HEVC HLS segment");
    fs::write(&ts_path, ts).expect("write HEVC transport stream");
    assert_probe_and_decode(&ts_path, "hevc");

    let mp4_path = dir.path().join("hevc-recording.mp4");
    let mut mp4_muxer = Mp4Muxer::new();
    for frame in &frames {
        mp4_muxer.push_frame(frame);
    }
    fs::write(&mp4_path, mp4_muxer.finalize()).expect("write HEVC MP4");
    assert_probe_and_decode(&mp4_path, "hevc");

    let fmp4_path = dir.path().join("hevc-fragmented.mp4");
    let mut fmp4_muxer = Fmp4Muxer::new(90_000);
    for frame in &frames {
        fmp4_muxer.push_frame(frame);
    }
    let init = fmp4_muxer.init_segment();
    let media = fmp4_muxer
        .flush_segment()
        .expect("flush HEVC fMP4 media segment");
    let mut fragmented = init.data.to_vec();
    fragmented.extend_from_slice(&media.data);
    fs::write(&fmp4_path, fragmented).expect("write HEVC fragmented MP4");
    assert_probe_and_decode(&fmp4_path, "hevc");
}
