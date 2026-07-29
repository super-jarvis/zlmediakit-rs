//! WHEP (WebRTC-HTTP Egress Protocol) playback glue.
//!
//! A browser (or any WHEP client) POSTs an SDP *offer*; we build a
//! `PeerConnection`, attach send-only tracks for the requested stream's video
//! (H.264) and audio (Opus), answer it, and pump the live `MediaSource` frames
//! into RTP packets carried over SRTP. Transport (ICE / DTLS / SRTP / SDP) is
//! handled entirely by the `webrtc` crate — this module only does the
//! media<->track mapping.
//!
//! Scope of the minimal skeleton:
//! * playback only (server -> client);
//! * H.264 video and Opus audio (the formats the wire payloaders in the
//!   `webrtc` crate understand natively). AAC audio is published by many
//!   streams but is not a WebRTC codec, so it is silently skipped for now.

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo};
use zlmediakit_core::media_source::MediaSource;

/// A live WHEP session. Held by the HTTP server so the `PeerConnection` (and the
/// background media pump) stay alive until the client issues a DELETE or the
/// server shuts down.
pub struct WhepSession {
    pub pc: Arc<RTCPeerConnection>,
    pub resource: String,
}

impl WhepSession {
    /// Tear down the PeerConnection and stop the media pump.
    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            warn!("webrtc: error closing pc for {}: {}", self.resource, e);
        }
    }
}

/// Negotiate a WHEP playback session.
///
/// * `offer_sdp`  — the SDP offer posted by the client.
/// * `source`     — the local stream to play.
/// * `resource`   — opaque id the HTTP layer uses for DELETE / bookkeeping.
///
/// Returns the SDP answer (to be returned to the client with the proper
/// `Content-Type` / `Location` headers) and the kept-alive session.
pub async fn whep_play(
    offer_sdp: &str,
    source: Arc<MediaSource>,
    resource: String,
) -> Result<(String, WhepSession)> {
    let mut m = MediaEngine::default();
    // Register exactly the codecs we can send. The `TrackLocalStaticSample`
    // binds to a matching registered codec to pick the right RTP payloader.
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
    )?;
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            stats_id: String::new(),
        },
        RTPCodecType::Audio,
    )?;

    let api = APIBuilder::new().with_media_engine(m).build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

    // Non-trickle WHEP: wait until ICE gathering completes so candidates are
    // embedded in the returned answer SDP.
    let gathering_done = Arc::new(Notify::new());
    let gathering_tx = gathering_done.clone();
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let notify = gathering_tx.clone();
        Box::pin(async move {
            if c.is_none() {
                notify.notify_waiters();
            }
        })
    }));

    // Decide which tracks the source actually carries.
    let (has_video, has_audio_opus) = {
        let info = source.info.read().await;
        let mut v = false;
        let mut a = false;
        for t in &info.tracks {
            match t {
                TrackInfo::Video(vi) if vi.codec == CodecId::H264 => v = true,
                TrackInfo::Audio(ai) if ai.codec == CodecId::Opus => a = true,
                _ => {}
            }
        }
        (v, a)
    };

    let mut video_track: Option<Arc<TrackLocalStaticSample>> = None;
    if has_video {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_string(),
                rtcp_feedback: vec![],
            },
            "video".to_string(),
            "zlmediakit".to_string(),
        ));
        let _ = pc.add_track(track.clone()).await;
        video_track = Some(track);
    }

    let mut audio_track: Option<Arc<TrackLocalStaticSample>> = None;
    if has_audio_opus {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![],
            },
            "audio".to_string(),
            "zlmediakit".to_string(),
        ));
        let _ = pc.add_track(track.clone()).await;
        audio_track = Some(track);
    }

    let offer = RTCSessionDescription::offer(offer_sdp.to_string())?;
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    // Block until ICE candidates are gathered and embedded in the answer.
    gathering_done.notified().await;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("webrtc: no local description after gathering"))?;
    let answer_sdp = local.sdp.clone();

    spawn_pump(source, video_track, audio_track);

    info!("webrtc: WHEP session {} negotiated", resource);
    Ok((answer_sdp, WhepSession { pc, resource }))
}

/// Spawns the background task that replays the cached GOP and then streams
/// live frames into the RTP tracks. Samples written before the DTLS/SRTP
/// transport is bound are dropped by `TrackLocalStaticSample` (it returns
/// `Ok(())` without sending), so starting immediately is safe.
fn spawn_pump(
    source: Arc<MediaSource>,
    video: Option<Arc<TrackLocalStaticSample>>,
    audio: Option<Arc<TrackLocalStaticSample>>,
) {
    tokio::spawn(async move {
        // Replay cached config (SPS/PPS) + latest GOP so a late decoder can
        // start from a key frame.
        let cached = source.get_latest_gop_frames().await;
        for f in &cached {
            push_frame(f, &video, &audio).await;
        }

        let mut rx = source.subscribe();
        loop {
            match rx.recv().await {
                Ok(f) => push_frame(&f, &video, &audio).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break, // closed
            }
        }
        info!("webrtc: media pump ended");
    });
}

async fn push_frame(
    f: &MediaFrame,
    video: &Option<Arc<TrackLocalStaticSample>>,
    audio: &Option<Arc<TrackLocalStaticSample>>,
) {
    match f.codec {
        CodecId::H264 => {
            if let Some(v) = video {
                let annexb = avcc_to_annexb(&f.data);
                if annexb.is_empty() {
                    return;
                }
                let sample = webrtc::media::Sample {
                    data: annexb,
                    timestamp: SystemTime::now(),
                    duration: Duration::from_millis(40),
                    ..Default::default()
                };
                if let Err(e) = v.write_sample(&sample).await {
                    debug!("webrtc: video write_sample error: {}", e);
                }
            }
        }
        CodecId::Opus => {
            if let Some(a) = audio {
                let sample = webrtc::media::Sample {
                    data: f.data.clone(),
                    timestamp: SystemTime::now(),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                };
                if let Err(e) = a.write_sample(&sample).await {
                    debug!("webrtc: audio write_sample error: {}", e);
                }
            }
        }
        _ => {}
    }
}

/// Convert an AVCC (length-prefixed) H.264 frame into Annex-B (start-code
/// separated) NALUs, which is what the `webrtc` crate's H.264 RTP payloader
/// expects. Handles both the normal sample (`avc_packet_type == 1`, 4-byte
/// lengths) and the decoder-configuration record (`avc_packet_type == 0`,
/// 2-byte lengths for SPS/PPS).
fn avcc_to_annexb(data: &[u8]) -> Bytes {
    if data.len() < 5 {
        return Bytes::copy_from_slice(data);
    }
    let avc_packet_type = data[1] & 0x0F;
    let mut out = BytesMut::new();
    const START: &[u8] = &[0x00, 0x00, 0x00, 0x01];

    if avc_packet_type == 0 {
        // AVCDecoderConfigurationRecord: [5]=version [6]=profile [7]=compat
        // [8]=level [9]=lenSizeMinusOne/reserved [10]=0xE0|numSPS
        if data.len() < 11 {
            return Bytes::copy_from_slice(data);
        }
        let num_sps = (data[10] & 0x1F) as usize;
        let mut pos = 11;
        for _ in 0..num_sps {
            if pos + 2 > data.len() {
                break;
            }
            let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + nalu_len > data.len() {
                break;
            }
            out.extend_from_slice(START);
            out.extend_from_slice(&data[pos..pos + nalu_len]);
            pos += nalu_len;
        }
        if pos < data.len() {
            let num_pps = data[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > data.len() {
                    break;
                }
                let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                if pos + nalu_len > data.len() {
                    break;
                }
                out.extend_from_slice(START);
                out.extend_from_slice(&data[pos..pos + nalu_len]);
                pos += nalu_len;
            }
        }
    } else {
        // Normal sample: 5-byte header then 4-byte length-prefixed NALUs.
        let mut pos = 5;
        while pos + 4 <= data.len() {
            let nalu_len =
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
            pos += 4;
            if pos + nalu_len > data.len() {
                break;
            }
            out.extend_from_slice(START);
            out.extend_from_slice(&data[pos..pos + nalu_len]);
            pos += nalu_len;
        }
    }
    out.freeze()
}
