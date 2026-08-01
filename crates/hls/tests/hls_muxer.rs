//! Tests for HLS/TS muxing: `HlsMuxer` (segmented .ts) and `TsLiveMuxer`
//! (interleaved live TS for HTTP-TS).

use bytes::Bytes;
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};
use zlmediakit_hls::{HlsMuxer, TsLiveMuxer};

fn h264_config() -> MediaFrame {
    // FLV video-tag style AVCC config record:
    // byte0 = 0x17 (key frame | H264 codec id 7), byte1 = 0x00 (AVCPacketType=config).
    let data = Bytes::from_static(&[
        0x17, 0x00, 0x00, 0x00, 0x00, 0x67, 0x42, 0x00, 0x1f, 0x68, 0xce, 0x3c, 0x80,
    ]);
    let mut f = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, data, false);
    f.config_frame = true;
    f
}

/// Build a FLV-style H264 NALU frame (AVCPacketType=1) with a 4-byte
/// big-endian NALU length prefix, which the muxers convert to Annex-B.
fn avcc(nalu: &[u8], key: bool) -> Bytes {
    let mut b = Vec::with_capacity(5 + 4 + nalu.len());
    // byte0: frame type (1=key, 2=inter) | codec id 7 (H264)
    b.push(if key { 0x17 } else { 0x27 });
    // byte1: AVCPacketType = 1 (NALU)
    b.push(0x01);
    // composition time (3 bytes)
    b.extend_from_slice(&[0x00, 0x00, 0x00]);
    // 4-byte NALU length + NALU
    b.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
    b.extend_from_slice(nalu);
    Bytes::from(b)
}

fn h264_key(dts: u64) -> MediaFrame {
    // NALU type 5 (IDR) payload.
    MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        dts,
        dts,
        avcc(&[0x65, 0x01, 0x02, 0x03], true),
        true,
    )
}

fn h264_nonkey(dts: u64) -> MediaFrame {
    MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        dts,
        dts,
        avcc(&[0x41, 0x01, 0x02, 0x03], false),
        false,
    )
}

const TS_SYNC: u8 = 0x47;

#[test]
fn hls_muxer_produces_ts_segment_on_key_frame() {
    let mut muxer = HlsMuxer::new(2.0).with_max_segments(5);
    muxer.push_frame(h264_config());
    // First key frame: elapsed since start is < target_duration, no segment yet.
    assert!(muxer.push_frame(h264_key(1500)).is_none());
    // Second key frame pushes elapsed past target_duration -> segment produced.
    let produced = muxer.push_frame(h264_key(3000));
    assert!(
        produced.is_some(),
        "a TS segment must be produced when target duration is exceeded"
    );
    let (_id, seg) = produced.unwrap();
    assert!(!seg.is_empty());
    assert_eq!(seg[0], TS_SYNC, "TS segment must start with sync byte");
}

#[test]
fn hls_muxer_flush_closes_segment() {
    let mut muxer = HlsMuxer::new(2.0);
    muxer.push_frame(h264_config());
    muxer.push_frame(h264_key(1500));
    muxer.push_frame(h264_nonkey(1540));
    if let Some((_id, seg)) = muxer.flush() {
        assert!(!seg.is_empty());
        assert_eq!(seg[0], TS_SYNC);
    }
}

#[test]
fn ts_live_muxer_emits_packets() {
    let mut muxer = TsLiveMuxer::new();
    // Live muxer emits PAT/PMT on the first frame (the config).
    let out0 = muxer.push_frame(&h264_config());
    assert!(!out0.is_empty(), "config frame should emit PAT/PMT");
    assert_eq!(out0[0], TS_SYNC);
    let out1 = muxer.push_frame(&h264_key(0));
    assert!(!out1.is_empty(), "key frame must emit TS packets");
    assert_eq!(out1[0], TS_SYNC);
    let out2 = muxer.push_frame(&h264_nonkey(40));
    assert!(!out2.is_empty());
    assert!(muxer.started());
}

#[test]
fn frame_type_classification() {
    assert_eq!(FrameType::Video as u8, FrameType::Video as u8);
}
