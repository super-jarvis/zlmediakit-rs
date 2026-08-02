use bytes::Bytes;
use zlmediakit_codec::ps::PsDemuxer;
use zlmediakit_core::{CodecId, MediaFrame, PayloadFormat, PsMuxer, TrackInfo, VideoInfo};

#[test]
fn core_ps_muxer_round_trips_through_codec_demuxer() {
    let tracks = [TrackInfo::Video(VideoInfo {
        codec: CodecId::H264,
        width: 1920,
        height: 1080,
        fps: 25.0,
        key_frame: true,
    })];
    let mut muxer = PsMuxer::new(&tracks);
    let frame = MediaFrame::new_video(
        0,
        CodecId::H264,
        1_000,
        1_000,
        1_000,
        Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
        true,
    )
    .with_payload_format(PayloadFormat::AnnexB);

    let program_stream = muxer.mux_frame(&frame).unwrap();
    let mut demuxer = PsDemuxer::new();
    demuxer.push(&program_stream);
    let pes = demuxer.next_pes().expect("muxed PS contains a video PES");

    assert_eq!(pes.stream_id, 0xe0);
    assert_eq!(pes.pts, Some(90_000));
    assert_eq!(pes.data.as_ref(), frame.data.as_ref());
    assert!(demuxer.next_pes().is_none());
    assert_eq!(demuxer.pending(), 0);
}
