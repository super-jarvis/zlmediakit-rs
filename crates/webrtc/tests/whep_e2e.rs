//! End-to-end check of the WHEP playback path without a browser.
//!
//! A synthetic H.264 stream is published into a `MediaSource`. A WHEP client
//! (built with the same `webrtc` crate) offers a recvonly session; the server
//! answers and starts pumping RTP. We assert the client actually receives SRTP
//! media packets — exercising ICE, DTLS, SRTP and the RTP payloader.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::{BufMut, Bytes};
use rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::{MediaSourceManager, TrackDescriptor};
use zlmediakit_webrtc::{pull_client, push_client, whep_play};

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
    zlmediakit_webrtc::init_crypto();
    // 1. Publish a synthetic H.264 stream.
    let mgr = Arc::new(MediaSourceManager::new(None));
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
                rtcp_feedback: vec![RTCPFeedback {
                    typ: "goog-remb".to_string(),
                    parameter: String::new(),
                }],
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
    let remote_ssrc = Arc::new(AtomicU32::new(0));
    {
        let received = received.clone();
        let remote_ssrc = remote_ssrc.clone();
        client_pc.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _recv: Arc<RTCRtpReceiver>,
                  _trans: Arc<RTCRtpTransceiver>| {
                let received = received.clone();
                let remote_ssrc = remote_ssrc.clone();
                Box::pin(async move {
                    remote_ssrc.store(track.ssrc(), Ordering::Relaxed);
                    let mut buf = vec![0u8; 1500];
                    while track.read(&mut buf).await.is_ok() {
                        let mut c = received.lock().await;
                        *c += 1;
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
    let (answer_sdp, session) = whep_play(&offer_sdp, source.clone(), "test".to_string(), vec![])
        .await
        .expect("whep_play should negotiate");
    assert_eq!(source.subscriber_count().await, 1);

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
                    avcc_sample(i.is_multiple_of(10), i),
                    i.is_multiple_of(10),
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
    let feedback: Vec<Box<dyn rtcp::packet::Packet + Send + Sync>> =
        vec![Box::new(ReceiverEstimatedMaximumBitrate {
            sender_ssrc: 1,
            bitrate: 750_000.0,
            ssrcs: vec![remote_ssrc.load(Ordering::Relaxed)],
        })];
    client_pc.write_rtcp(&feedback).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while session.estimated_bitrate_bps() != 750_000 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server should observe negotiated REMB feedback");
    session.close().await;
    assert_eq!(source.subscriber_count().await, 0);
    println!("WHEP e2e OK: received {} RTP packets", count);
}

/// ---------------------------------------------------------------------------
/// Second test: the *real* HTTP WHEP server (crates/webrtc/src/server.rs), so the
/// request parsing / body reading path is exercised too, not just `whep_play`.
/// ---------------------------------------------------------------------------
use zlmediakit_webrtc::{WebRtcServer, WebRtcTransportConfig};

const TEST_HTTP_PORT: u16 = 19123;

fn http_content_length(buf: &[u8]) -> usize {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let text = String::from_utf8_lossy(&buf[..head_end]);
    for line in text.lines() {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

fn http_parse(buf: &[u8]) -> (u16, String, String) {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let text = String::from_utf8_lossy(&buf[..head_end]);
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let mut location = String::new();
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("location:") {
            location = v.trim().to_string();
        }
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    (status, body, location)
}

async fn http_exchange(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body_str = body.unwrap_or("");
    let req = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method, path, body_str.len(), body_str
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                let he = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                if let Some(h) = he {
                    if buf.len() >= h + http_content_length(&buf) {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    http_parse(&buf)
}

fn ice_sdp_fragment(sdp: &str) -> String {
    let username_fragment = sdp
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .unwrap();
    let password = sdp
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-pwd:"))
        .unwrap();
    let mut output = format!("a=ice-ufrag:{username_fragment}\r\na=ice-pwd:{password}\r\n");
    let mut in_media = false;
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            in_media = true;
            output.push_str(line);
            output.push_str("\r\n");
        } else if in_media
            && (line.starts_with("a=mid:")
                || line.starts_with("a=candidate:")
                || line == "a=end-of-candidates")
        {
            output.push_str(line);
            output.push_str("\r\n");
        }
    }
    output
}

fn apply_ice_sdp_fragment(current_sdp: &str, fragment: &str) -> String {
    let username_fragment = fragment
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .unwrap();
    let password = fragment
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-pwd:"))
        .unwrap();
    let mut candidates = HashMap::<String, Vec<String>>::new();
    let mut current_mid = None;
    for line in fragment.lines().map(str::trim) {
        if let Some(mid) = line.strip_prefix("a=mid:") {
            current_mid = Some(mid.to_string());
        } else if (line.starts_with("a=candidate:") || line == "a=end-of-candidates")
            && current_mid.is_some()
        {
            candidates
                .entry(current_mid.clone().unwrap())
                .or_default()
                .push(line.to_string());
        }
    }

    let mut output = String::with_capacity(current_sdp.len() + fragment.len());
    for line in current_sdp.lines().map(str::trim) {
        if line.starts_with("a=ice-ufrag:") {
            output.push_str(&format!("a=ice-ufrag:{username_fragment}\r\n"));
        } else if line.starts_with("a=ice-pwd:") {
            output.push_str(&format!("a=ice-pwd:{password}\r\n"));
        } else if line.starts_with("a=candidate:") || line == "a=end-of-candidates" {
            continue;
        } else {
            output.push_str(line);
            output.push_str("\r\n");
            if let Some(mid) = line.strip_prefix("a=mid:") {
                if let Some(lines) = candidates.get(mid) {
                    for candidate in lines {
                        output.push_str(candidate);
                        output.push_str("\r\n");
                    }
                }
            }
        }
    }
    output
}

#[tokio::test]
async fn whep_http_e2e_receives_media() {
    zlmediakit_webrtc::init_crypto();
    let mgr = Arc::new(MediaSourceManager::new(None));
    let source = mgr.get_or_create("__defaultVhost__", "live", "webrtchttp");
    source
        .update_tracks(vec![
            TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 1280,
                height: 720,
                fps: 25.0,
                key_frame: false,
            }),
            TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 640,
                height: 360,
                fps: 15.0,
                key_frame: false,
            }),
        ])
        .await;
    source
        .set_track_descriptor(
            0,
            TrackDescriptor {
                rid: Some("h".to_string()),
                ..Default::default()
            },
        )
        .await;
    source
        .set_track_descriptor(
            1,
            TrackDescriptor {
                rid: Some("l".to_string()),
                ..Default::default()
            },
        )
        .await;
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

    // Start the real WHEP HTTP server.
    let srv = WebRtcServer::new_with_transport(
        &format!("127.0.0.1:{}", TEST_HTTP_PORT),
        mgr.clone(),
        vec![],
        WebRtcTransportConfig {
            // A specific bind IP also constrains the advertised host candidate,
            // so the SDP cannot point at an interface the mux did not bind.
            udp_bind_addr: "127.0.0.1:0".parse().unwrap(),
            ice_lite: false,
            external_ips: vec![],
        },
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    // Build a WHEP client (recvonly) and produce an offer.
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
                    while track.read(&mut buf).await.is_ok() {
                        let mut c = received.lock().await;
                        *c += 1;
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

    let (missing_status, _, _) = http_exchange(
        TEST_HTTP_PORT,
        "POST",
        "/webrtc/play/__defaultVhost__/live/webrtchttp?rid=missing",
        Some(&offer_sdp),
    )
    .await;
    assert_eq!(
        missing_status, 404,
        "unknown simulcast RID must be rejected"
    );

    // POST the offer to the HTTP WHEP endpoint.
    let (status, answer_sdp, location) = http_exchange(
        TEST_HTTP_PORT,
        "POST",
        "/webrtc/play/__defaultVhost__/live/webrtchttp?rid=h",
        Some(&offer_sdp),
    )
    .await;
    assert_eq!(status, 201, "WHEP POST should return 201");
    assert!(!answer_sdp.is_empty(), "answer SDP must be returned");
    assert!(!location.is_empty(), "Location header must be returned");

    let remote_ufrag = offer_sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap_or_default();
    let trickle_fragment = format!(
        "a=ice-ufrag:{remote_ufrag}\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
a=mid:0\r\n\
a=candidate:999 1 udp 1 192.0.2.1 9 typ host\r\n"
    );
    let (patch_status, _, _) =
        http_exchange(TEST_HTTP_PORT, "PATCH", &location, Some(&trickle_fragment)).await;
    assert_eq!(patch_status, 204, "trickle ICE PATCH should be accepted");

    let answer = RTCSessionDescription::answer(answer_sdp).unwrap();
    client_pc.set_remote_description(answer).await.unwrap();

    // Keep publishing so media keeps flowing.
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
                    avcc_sample(i.is_multiple_of(10), i),
                    i.is_multiple_of(10),
                ))
                .await;
            i += 1;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });

    // Wait for media.
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
        "HTTP WHEP client should receive RTP media; got {}",
        count
    );

    // Perform a real WHEP ICE restart over PATCH. Both peers must replace
    // their ICE credentials, complete a new offer/answer exchange, and keep
    // the existing media session flowing afterward.
    let old_local = client_pc.local_description().await.unwrap();
    let old_remote = client_pc.remote_description().await.unwrap();
    let old_username_fragment = old_local
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap()
        .to_string();
    let restart_offer = client_pc
        .create_offer(Some(RTCOfferOptions {
            ice_restart: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut restart_gathering = client_pc.gathering_complete_promise().await;
    client_pc
        .set_local_description(restart_offer)
        .await
        .unwrap();
    timeout(Duration::from_secs(10), restart_gathering.recv())
        .await
        .expect("client ICE restart gathering timed out");
    let new_local = client_pc.local_description().await.unwrap();
    let new_username_fragment = new_local
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap();
    assert_ne!(old_username_fragment, new_username_fragment);

    let restart_fragment = ice_sdp_fragment(&new_local.sdp);
    let (restart_status, answer_fragment, _) =
        http_exchange(TEST_HTTP_PORT, "PATCH", &location, Some(&restart_fragment)).await;
    assert_eq!(restart_status, 200, "ICE restart PATCH should return SDP");
    let restarted_answer = apply_ice_sdp_fragment(&old_remote.sdp, &answer_fragment);
    client_pc
        .set_remote_description(RTCSessionDescription::answer(restarted_answer).unwrap())
        .await
        .unwrap();

    timeout(Duration::from_secs(10), async {
        loop {
            if *received.lock().await >= count + 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("media did not resume after ICE restart");

    // DELETE to tear down.
    let (del_status, _, _) = http_exchange(TEST_HTTP_PORT, "DELETE", &location, None).await;
    assert_eq!(del_status, 200, "WHEP DELETE should return 200");

    println!("WHEP HTTP e2e OK: received {} RTP packets", count);
}

// ---------------------------------------------------------------------------
// Third test: full WHIP (publish) -> MediaSource -> WHEP (play) loopback.
// Exercises the H.264 RTP *de*payloader (FU-A reassembly + AVCC conversion)
// and the WHIP HTTP endpoint end to end, without a browser.
// ---------------------------------------------------------------------------

const TEST_WHIP_HTTP_PORT: u16 = 19124;

fn h264_cap() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: webrtc::api::media_engine::MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_string(),
        rtcp_feedback: vec![],
    }
}

fn opus_cap() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
        rtcp_feedback: vec![],
    }
}

fn h264_params(pt: u8) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: h264_cap(),
        payload_type: pt,
        stats_id: String::new(),
    }
}

fn opus_params(pt: u8) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: opus_cap(),
        payload_type: pt,
        stats_id: String::new(),
    }
}

fn annexb_frame(nalu_type: u8, payload: &[u8]) -> Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    b.put_u8(nalu_type);
    b.extend_from_slice(payload);
    b.freeze()
}

#[tokio::test]
async fn whip_e2e_publish_then_play() {
    zlmediakit_webrtc::init_crypto();
    let mgr = Arc::new(MediaSourceManager::new(None));
    let vhost = "__defaultVhost__";
    let app = "live";
    let stream = "whiploop";

    let srv = WebRtcServer::new(
        &format!("127.0.0.1:{}", TEST_WHIP_HTTP_PORT),
        mgr.clone(),
        vec![],
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    // ---- WHIP publisher (browser-equivalent, sendonly) ----
    let mut pm = MediaEngine::default();
    pm.register_codec(h264_params(102), RTPCodecType::Video)
        .unwrap();
    pm.register_codec(opus_params(111), RTPCodecType::Audio)
        .unwrap();
    let papi = APIBuilder::new().with_media_engine(pm).build();
    let pub_pc = Arc::new(
        papi.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let v_track = Arc::new(TrackLocalStaticSample::new(
        h264_cap(),
        "video".into(),
        "zlm".into(),
    ));
    let a_track = Arc::new(TrackLocalStaticSample::new(
        opus_cap(),
        "audio".into(),
        "zlm".into(),
    ));
    let _ = pub_pc.add_track(v_track.clone()).await;
    let _ = pub_pc.add_track(a_track.clone()).await;

    let pub_gather = Arc::new(Notify::new());
    {
        let g = pub_gather.clone();
        pub_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let g = g.clone();
            Box::pin(async move {
                if c.is_none() {
                    g.notify_waiters();
                }
            })
        }));
    }
    let offer = pub_pc.create_offer(None).await.unwrap();
    pub_pc.set_local_description(offer).await.unwrap();
    pub_gather.notified().await;
    let offer_sdp = pub_pc.local_description().await.unwrap().sdp.clone();

    let (pstatus, answer_sdp, ploc) = http_exchange(
        TEST_WHIP_HTTP_PORT,
        "POST",
        &format!("/webrtc/publish/{}/{}/{}", vhost, app, stream),
        Some(&offer_sdp),
    )
    .await;
    assert_eq!(pstatus, 201, "WHIP POST should return 201");
    let answer = RTCSessionDescription::answer(answer_sdp).unwrap();
    pub_pc.set_remote_description(answer).await.unwrap();

    // Publisher pushes H.264 (with periodic IDR) + Opus samples. The 2000-byte
    // H.264 payload forces FU-A fragmentation so the server's depayloader is
    // exercised.
    let v_src = v_track.clone();
    let a_src = a_track.clone();
    tokio::spawn(async move {
        let mut i = 0u32;
        loop {
            let nalu = if i.is_multiple_of(10) { 5u8 } else { 1u8 };
            let sample = Sample {
                data: annexb_frame(nalu, &[0xAA; 2000]),
                timestamp: SystemTime::now(),
                duration: Duration::from_millis(40),
                ..Default::default()
            };
            let _ = v_src.write_sample(&sample).await;
            let opus = Sample {
                data: Bytes::from(vec![0xBB; 40]),
                timestamp: SystemTime::now(),
                duration: Duration::from_millis(20),
                ..Default::default()
            };
            let _ = a_src.write_sample(&opus).await;
            i += 1;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });

    // Let the server receive + publish into the MediaSource.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // ---- WHEP player (browser-equivalent, recvonly) ----
    let mut wm = MediaEngine::default();
    wm.register_codec(h264_params(102), RTPCodecType::Video)
        .unwrap();
    wm.register_codec(opus_params(111), RTPCodecType::Audio)
        .unwrap();
    let wapi = APIBuilder::new().with_media_engine(wm).build();
    let play_pc = Arc::new(
        wapi.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );

    let received = Arc::new(Mutex::new(0u32));
    {
        let received = received.clone();
        play_pc.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _recv: Arc<RTCRtpReceiver>,
                  _trans: Arc<RTCRtpTransceiver>| {
                let received = received.clone();
                Box::pin(async move {
                    let mut buf = vec![0u8; 2000];
                    while track.read(&mut buf).await.is_ok() {
                        let mut c = received.lock().await;
                        *c += 1;
                    }
                })
            },
        ));
    }
    let play_gather = Arc::new(Notify::new());
    {
        let g = play_gather.clone();
        play_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let g = g.clone();
            Box::pin(async move {
                if c.is_none() {
                    g.notify_waiters();
                }
            })
        }));
    }
    play_pc
        .add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
    play_pc
        .add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
    let offer = play_pc.create_offer(None).await.unwrap();
    play_pc.set_local_description(offer).await.unwrap();
    play_gather.notified().await;
    let offer_sdp = play_pc.local_description().await.unwrap().sdp.clone();

    let (wstatus, answer_sdp, wloc) = http_exchange(
        TEST_WHIP_HTTP_PORT,
        "POST",
        &format!("/webrtc/play/{}/{}/{}", vhost, app, stream),
        Some(&offer_sdp),
    )
    .await;
    assert_eq!(wstatus, 201, "WHEP POST should return 201");
    let answer = RTCSessionDescription::answer(answer_sdp).unwrap();
    play_pc.set_remote_description(answer).await.unwrap();

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
        "WHEP player should receive RTP from WHIP-published stream; got {}",
        count
    );

    let (del, _, _) = http_exchange(TEST_WHIP_HTTP_PORT, "DELETE", &wloc, None).await;
    assert_eq!(del, 200, "WHEP DELETE should return 200");
    let (del, _, _) = http_exchange(TEST_WHIP_HTTP_PORT, "DELETE", &ploc, None).await;
    assert_eq!(del, 200, "WHIP DELETE should return 200");
    assert!(
        mgr.get(vhost, app, stream).is_none(),
        "WHIP DELETE should remove the published MediaSource"
    );

    println!("WHIP->WHEP loopback OK: received {} RTP packets", count);
}

#[tokio::test]
async fn native_whep_pull_republishes_remote_media() {
    zlmediakit_webrtc::init_crypto();

    let upstream_manager = Arc::new(MediaSourceManager::new(None));
    let upstream = upstream_manager.get_or_create("__defaultVhost__", "live", "remote");
    upstream
        .update_tracks(vec![TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: false,
        })])
        .await;
    upstream
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

    // Exercise the native WHEP client's bracketed IPv6 authority and the
    // signalling listener over a real IPv6 TCP connection.
    let server = WebRtcServer::new("[::1]:0", upstream_manager, vec![])
        .await
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move { server.run().await });

    let downstream_manager = Arc::new(MediaSourceManager::new(None));
    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let pull_task = {
        let downstream_manager = downstream_manager.clone();
        let stop = stop.clone();
        let stopped = stopped.clone();
        tokio::spawn(async move {
            pull_client::start(
                &format!("whep://[::1]:{port}/webrtc/play/__defaultVhost__/live/remote"),
                "__defaultVhost__",
                "live",
                "local",
                downstream_manager,
                vec![],
                stop,
                stopped,
            )
            .await
        })
    };

    let publisher = tokio::spawn(async move {
        for i in 1..=100u32 {
            upstream
                .publish_and_cache(MediaFrame::new_video(
                    0,
                    CodecId::H264,
                    i,
                    i as u64 * 40,
                    i as u64 * 40,
                    avcc_sample(i.is_multiple_of(25), i),
                    i.is_multiple_of(25),
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });

    timeout(Duration::from_secs(15), async {
        loop {
            if let Some(source) =
                downstream_manager.get("__defaultVhost__", "live", "local")
            {
                let frames = source.get_latest_gop_frames().await;
                if frames
                    .iter()
                    .any(|frame| frame.codec == CodecId::H264 && !frame.config_frame)
                {
                    assert!(source.info.read().await.tracks.iter().any(
                        |track| matches!(track, TrackInfo::Video(video) if video.codec == CodecId::H264)
                    ));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("native WHEP pull did not republish H.264 media");

    stopped.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    timeout(Duration::from_secs(10), pull_task)
        .await
        .expect("native WHEP pull did not stop")
        .expect("native WHEP pull task panicked")
        .expect("native WHEP pull failed");
    publisher.abort();
    server_task.abort();
}

#[tokio::test]
async fn native_whip_push_publishes_remote_media() {
    zlmediakit_webrtc::init_crypto();

    let remote_manager = Arc::new(MediaSourceManager::new(None));
    let server = WebRtcServer::new("127.0.0.1:0", remote_manager.clone(), vec![])
        .await
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move { server.run().await });

    let local_manager = Arc::new(MediaSourceManager::new(None));
    let local = local_manager.get_or_create("__defaultVhost__", "live", "local");
    local
        .update_tracks(vec![TrackInfo::Video(VideoInfo {
            codec: CodecId::H264,
            width: 1280,
            height: 720,
            fps: 25.0,
            key_frame: false,
        })])
        .await;
    local
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

    let stop = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let push_task = {
        let local_manager = local_manager.clone();
        let stop = stop.clone();
        let stopped = stopped.clone();
        tokio::spawn(async move {
            push_client::start(
                &format!("whip://127.0.0.1:{port}/webrtc/publish/__defaultVhost__/live/remote"),
                "__defaultVhost__",
                "live",
                "local",
                local_manager,
                vec![],
                stop,
                stopped,
            )
            .await
        })
    };

    let publisher = tokio::spawn(async move {
        for i in 1..=100u32 {
            local
                .publish_and_cache(MediaFrame::new_video(
                    0,
                    CodecId::H264,
                    i,
                    i as u64 * 40,
                    i as u64 * 40,
                    avcc_sample(i.is_multiple_of(25), i),
                    i.is_multiple_of(25),
                ))
                .await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });

    timeout(Duration::from_secs(15), async {
        loop {
            if let Some(source) = remote_manager.get("__defaultVhost__", "live", "remote") {
                if source
                    .get_latest_gop_frames()
                    .await
                    .iter()
                    .any(|frame| frame.codec == CodecId::H264 && !frame.config_frame)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("native WHIP push did not publish H.264 media remotely");

    stopped.store(true, Ordering::SeqCst);
    stop.notify_waiters();
    timeout(Duration::from_secs(10), push_task)
        .await
        .expect("native WHIP push did not stop")
        .expect("native WHIP push task panicked")
        .expect("native WHIP push failed");
    assert!(
        remote_manager
            .get("__defaultVhost__", "live", "remote")
            .is_none(),
        "WHIP DELETE should remove the remote MediaSource"
    );
    publisher.abort();
    server_task.abort();
}
