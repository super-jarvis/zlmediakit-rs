//! End-to-end check of the WHEP playback path without a browser.
//!
//! A synthetic H.264 stream is published into a `MediaSource`. A WHEP client
//! (built with the same `webrtc` crate) offers a recvonly session; the server
//! answers and starts pumping RTP. We assert the client actually receives SRTP
//! media packets — exercising ICE, DTLS, SRTP and the RTP payloader.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_remote::TrackRemote;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_webrtc::whep_play;

fn avcc_config() -> Bytes {
    // AVCDecoderConfigurationRecord (avc_packet_type == 0): SPS(10) + PPS(4)
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17,
        0x00,
        0x00,
        0x00,
        0x00, // header, avc_packet_type = 0
        0x01,
        0x42,
        0x00,
        0x1f,
        0xff,
        0xe0 | 1, // version/profile/level, numSPS
        0x00,
        0x0a, // SPS length (2 bytes)
    ];
    v.extend_from_slice(&sps);
    v.push(0x01); // numPPS
    v.extend_from_slice(&[0x00, 0x04]); // PPS length
    v.extend_from_slice(&pps);
    Bytes::from(v)
}

fn avcc_sample(key: bool, seq: u32) -> Bytes {
    // avc_packet_type == 1, single 4-byte-length-prefixed NALU.
    let nalu_type: u8 = if key { 0x65 } else { 0x41 };
    let nalu = [nalu_type, 0x9a, 0x00, 0x10 + (seq % 200) as u8, 0x20];
    let mut v = vec![
        0x17u8 | if key { 0x00 } else { 0x10 },
        0x01,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x05, // NALU length (4 bytes) = 5
    ];
    v.extend_from_slice(&nalu);
    Bytes::from(v)
}

#[tokio::test]
async fn whep_e2e_receives_media() {
    // 1. Publish a synthetic H.264 stream.
    let mgr = Arc::new(MediaSourceManager::new());
    let source = mgr.get_or_create("__defaultVhost__", "live", "webrtctest");
    source
        .update_tracks(vec![TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: false,
        })])
        .await;

    // Seed the GOP cache (config + a few frames) BEFORE negotiation so the
    // server pump replays them immediately.
    source
        .publish_and_cache(MediaFrame::new_video(
            0,
            CodecId::H264,
            0,
            0,
            0,
            avcc_config(),
            true,
        ))
        .await;
    for i in 1..=5u32 {
        source
            .publish_and_cache(MediaFrame::new_video(
                0,
                CodecId::H264,
                i,
                i as u64,
                i as u64,
                avcc_sample(i % 10 == 0, i),
                i % 10 == 0,
            ))
            .await;
    }

    // 2. Build the WHEP client (recvonly video).
    let mut m = MediaEngine::default();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
            stats_id: String::new(),
        },
        RTPCodecType::Video,
    )
    .unwrap();
    let api = APIBuilder::new().with_media_engine(m).build();
    let client_pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );

    let received = Arc::new(Mutex::new(0u32));
    {
        let received = received.clone();
        client_pc.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _recv: Arc<RTCRtpReceiver>,
                  _trans: Arc<RTCRtpTransceiver>| {
                let received = received.clone();
                Box::pin(async move {
                    let mut buf = vec![0u8; 1500];
                    loop {
                        match track.read(&mut buf).await {
                            Ok(_) => {
                                let mut c = received.lock().await;
                                *c += 1;
                            }
                            Err(_) => break,
                        }
                    }
                })
            },
        ));
    }

    let client_gather = Arc::new(Notify::new());
    {
        let g = client_gather.clone();
        client_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let g = g.clone();
            Box::pin(async move {
                if c.is_none() {
                    g.notify_waiters();
                }
            })
        }));
    }

    client_pc
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();

    let offer = client_pc.create_offer(None).await.unwrap();
    client_pc.set_local_description(offer).await.unwrap();
    client_gather.notified().await;
    let offer_sdp = client_pc.local_description().await.unwrap().sdp.clone();

    // 3. Server answers and starts pumping.
    let (answer_sdp, _session) = whep_play(&offer_sdp, source.clone(), "test".to_string())
        .await
        .expect("whep_play should negotiate");

    let answer = RTCSessionDescription::answer(answer_sdp).unwrap();
    client_pc.set_remote_description(answer).await.unwrap();

    // Keep publishing so the client keeps receiving after the initial GOP.
    let pub_src = source.clone();
    tokio::spawn(async move {
        let mut i = 6u32;
        loop {
            pub_src
                .publish_and_cache(MediaFrame::new_video(
                    0,
                    CodecId::H264,
                    i,
                    i as u64,
                    i as u64,
                    avcc_sample(i % 10 == 0, i),
                    i % 10 == 0,
                ))
                .await;
            i += 1;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });

    // 4. Wait for media to flow.
    let result = timeout(Duration::from_secs(10), async {
        loop {
            if *received.lock().await >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    let count = *received.lock().await;
    assert!(
        result.is_ok(),
        "client should receive RTP media; got {}",
        count
    );
    println!("WHEP e2e OK: received {} RTP packets", count);
}
