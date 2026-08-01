//! Tests for `FlvMuxer` — converting `MediaFrame`s into FLV tags.

use bytes::Bytes;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_flv::FlvMuxer;

fn h264_config() -> MediaFrame {
    let data = Bytes::from_static(&[0x67, 0x42, 0x00, 0x1f, 0x68, 0xce, 0x3c, 0x80]);
    let mut f = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, data, false);
    f.config_frame = true;
    f
}

fn h264_key(pts: u64) -> MediaFrame {
    let data = Bytes::from_static(&[0x65, 0x01, 0x02, 0x03]);
    MediaFrame::new_video(0, CodecId::H264, 0, pts, pts, data, true)
}

fn aac_config() -> MediaFrame {
    let data = Bytes::from_static(&[0x12, 0x08]);
    let mut f = MediaFrame::new_audio(1, CodecId::AAC, 0, 0, 0, data);
    f.config_frame = true;
    f
}

#[test]
fn flv_header_is_f_present() {
    let mut muxer = FlvMuxer::new();
    let header = muxer.write_header();
    assert!(header.len() >= 9);
    assert_eq!(&header[..3], b"FLV");
    // Flags: audio(0x04) | video(0x01)
    assert!(header[4] & 0x01 != 0, "video flag should be set");
}

#[test]
fn flv_tags_emitted_for_video_audio() {
    let mut muxer = FlvMuxer::new();
    muxer.write_header();

    let cfg_tag = muxer.write_tag(&h264_config());
    assert!(!cfg_tag.is_empty());
    assert_eq!(cfg_tag[0], 9, "video tag type is 9"); // TagType = video

    let key_tag = muxer.write_tag(&h264_key(0));
    assert_eq!(key_tag[0], 9);

    let aac_tag = muxer.write_tag(&aac_config());
    assert_eq!(aac_tag[0], 8, "audio tag type is 8");
}

#[test]
fn flv_metadata_tag() {
    let muxer = FlvMuxer::new();
    let meta = muxer.write_metadata(1280, 720, 25.0, 48000, CodecId::H264);
    assert!(!meta.is_empty());
}
