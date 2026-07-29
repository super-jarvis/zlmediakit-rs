//! WHIP (WebRTC-HTTP Ingestion Protocol) publishing glue.
//!
//! A browser (or any WHIP client) POSTs an SDP *offer* (sendonly); we answer
//! recvonly, receive the publisher's RTP streams, depacketize them, and
//! publish the resulting `MediaFrame`s into the shared `MediaSource` so any
//! other protocol (RTMP / FLV / HLS / WHEP) can play the stream.
//!
//! Transport (ICE / DTLS / SRTP / SDP) is handled by the `webrtc` crate; this
//! module only does the RTP->`MediaFrame` mapping and the Annex-B->AVCC
//! conversion the rest of the pipeline expects.

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
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
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::media_frame::{AudioInfo, CodecId, MediaFrame, TrackInfo, VideoInfo};
use zlmediakit_core::media_source::MediaSource;

use crate::h264_rtp::H264RtpDepacketizer;
use crate::whep::build_rtc_config;

const H264_MIME: &str = webrtc::api::media_engine::MIME_TYPE_H264;
const OPUS_MIME: &str = webrtc::api::media_engine::MIME_TYPE_OPUS;
const H264_CAP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const OPUS_CAP: &str = "minptime=10;useinbandfec=1";

/// A live WHIP session. Held by the HTTP server until the client issues a
/// DELETE or the server shuts down.
pub struct WhipSession {
    pub pc: Arc<RTCPeerConnection>,
    pub resource: String,
}

impl WhipSession {
    /// Tear down the PeerConnection.
    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            warn!("webrtc: error closing whip pc for {}: {}", self.resource, e);
        }
    }
}

/// Negotiate a WHIP publishing session.
///
/// * `offer_sdp` — the SDP offer POSTed by the publisher (sendonly).
/// * `source`    — the local stream to publish into.
/// * `resource`  — opaque id the HTTP layer uses for DELETE / bookkeeping.
///
/// Returns the SDP answer and the kept-alive session.
pub async fn whip_publish(
    offer_sdp: &str,
    source: Arc<MediaSource>,
    resource: String,
    ice_servers: Vec<IceServer>,
) -> Result<(String, WhipSession)> {
    let mut m = MediaEngine::default();
    m.register_codec(
        codec_params(H264_MIME, 90_000, 0, H264_CAP, 102),
        RTPCodecType::Video,
    )?;
    m.register_codec(
        codec_params(OPUS_MIME, 48_000, 2, OPUS_CAP, 111),
        RTPCodecType::Audio,
    )?;

    let api = APIBuilder::new().with_media_engine(m).build();
    let cfg = build_rtc_config(&ice_servers);
    let pc = Arc::new(api.new_peer_connection(cfg).await?);

    // Advertise what this stream carries so a later WHEP player creates the
    // matching send tracks. A browser WHIP publisher typically sends H.264 +
    // Opus; if it sends something else the extra track simply stays idle.
    source
        .update_tracks(vec![
            TrackInfo::Video(VideoInfo {
                codec: CodecId::H264,
                width: 0,
                height: 0,
                fps: 0.0,
                key_frame: false,
            }),
            TrackInfo::Audio(AudioInfo {
                codec: CodecId::Opus,
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 16,
            }),
        ])
        .await;

    let gathering_done = Arc::new(Notify::new());
    {
        let g = gathering_done.clone();
        pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let g = g.clone();
            Box::pin(async move {
                if c.is_none() {
                    g.notify_waiters();
                }
            })
        }));
    }

    let src = source.clone();
    pc.on_track(Box::new(
        move |track: Arc<TrackRemote>, _recv: Arc<RTCRtpReceiver>, trans: Arc<RTCRtpTransceiver>| {
            let src = src.clone();
            let kind = trans.kind();
            Box::pin(async move {
                match kind {
                    RTPCodecType::Video => receive_video(track, src).await,
                    RTPCodecType::Audio => receive_audio(track, src).await,
                    _ => debug!("webrtc: whip ignoring track of unknown kind"),
                }
            })
        },
    ));

    let offer = RTCSessionDescription::offer(offer_sdp.to_string())?;
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    // Non-trickle WHIP: wait until ICE candidates are gathered and embedded in
    // the returned answer SDP.
    gathering_done.notified().await;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("webrtc: no local description after gathering"))?;
    let answer_sdp = local.sdp.clone();

    info!("webrtc: WHIP session {} negotiated", resource);
    Ok((answer_sdp, WhipSession { pc, resource }))
}

fn codec_params(mime: &str, clock: u32, channels: u8, fmtp: &str, pt: u8) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: mime.to_owned(),
            clock_rate: clock,
            channels,
            sdp_fmtp_line: fmtp.to_string(),
            rtcp_feedback: vec![],
        },
        payload_type: pt,
        stats_id: String::new(),
    }
}

async fn receive_video(track: Arc<TrackRemote>, source: Arc<MediaSource>) {
    let mut dep = H264RtpDepacketizer::new();
    let mut sps: Option<Bytes> = None;
    let mut pps: Option<Bytes> = None;
    let mut config_sent = false;
    let mut buf = vec![0u8; 4096];
    loop {
        let pkt = match track.read(&mut buf).await {
            Ok((p, _)) => p,
            Err(_) => break,
        };
        let ts = pkt.header.timestamp as u64;
        let seq = pkt.header.sequence_number as u32;
        let Some(annexb) = dep.push(&pkt.payload) else {
            continue;
        };
        let nalus = split_nalus(&annexb);
        for (nt, nalu) in &nalus {
            if *nt == 7 {
                sps = Some(nalu.clone());
            } else if *nt == 8 {
                pps = Some(nalu.clone());
            }
        }
        let key = nalus.iter().any(|(nt, _)| *nt == 5);
        let has_vcl = nalus.iter().any(|(nt, _)| (1..=5).contains(nt));

        if !has_vcl {
            // SPS/PPS only: emit the AVCC decoder config once both are known.
            if sps.is_some() && pps.is_some() {
                if let Some(cfg) = build_avcc_config(sps.as_deref(), pps.as_deref()) {
                    let mut f =
                        MediaFrame::new_video(0, CodecId::H264, seq, ts, ts, cfg, true);
                    f.config_frame = true;
                    source.publish_and_cache(f).await;
                    config_sent = true;
                }
            }
        } else {
            // VCL frame: ensure the decoder config was sent first.
            if !config_sent {
                if let Some(cfg) = build_avcc_config(sps.as_deref(), pps.as_deref()) {
                    let mut f =
                        MediaFrame::new_video(0, CodecId::H264, seq, ts, ts, cfg, true);
                    f.config_frame = true;
                    source.publish_and_cache(f).await;
                    config_sent = true;
                }
            }
            if let Some(sample) = build_avcc_sample(&annexb, key) {
                source
                    .publish_and_cache(MediaFrame::new_video(
                        0, CodecId::H264, seq, ts, ts, sample, key,
                    ))
                    .await;
            }
        }
    }
    info!("webrtc: whip video receiver ended");
}

async fn receive_audio(track: Arc<TrackRemote>, source: Arc<MediaSource>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let pkt = match track.read(&mut buf).await {
            Ok((p, _)) => p,
            Err(_) => break,
        };
        let ts = pkt.header.timestamp as u64;
        let seq = pkt.header.sequence_number as u32;
        // Opus RTP payload is the raw Opus frame(s); forward as-is so a WHEP
        // player can repacketize it.
        source
            .publish_and_cache(MediaFrame::new_audio(
                1,
                CodecId::Opus,
                seq,
                ts,
                ts,
                Bytes::copy_from_slice(&pkt.payload),
            ))
            .await;
    }
    info!("webrtc: whip audio receiver ended");
}

/// Split an Annex-B buffer (start-code separated NALUs) into (nalu_type, nalu).
fn split_nalus(annexb: &[u8]) -> Vec<(u8, Bytes)> {
    let mut out = Vec::new();
    let n = annexb.len();
    let mut i = 0;
    while i < n {
        if i + 4 <= n && &annexb[i..i + 4] == [0, 0, 0, 1] {
            i += 4;
        } else if i + 3 <= n && &annexb[i..i + 3] == [0, 0, 1] {
            i += 3;
        } else {
            i += 1;
            continue;
        }
        let start = i;
        while i < n {
            if i + 4 <= n && &annexb[i..i + 4] == [0, 0, 0, 1] {
                break;
            }
            if i + 3 <= n && &annexb[i..i + 3] == [0, 0, 1] {
                break;
            }
            i += 1;
        }
        let end = i;
        if end > start {
            let nalu = &annexb[start..end];
            if !nalu.is_empty() {
                out.push((nalu[0] & 0x1F, Bytes::copy_from_slice(nalu)));
            }
        }
    }
    out
}

/// Build an AVCC `AVCDecoderConfigurationRecord` (FLV decoder-config layout)
/// from SPS/PPS. Returns `None` if either is missing.
fn build_avcc_config(sps: Option<&[u8]>, pps: Option<&[u8]>) -> Option<Bytes> {
    let (sps, pps) = (sps?, pps?);
    if sps.len() < 4 || pps.len() < 2 {
        return None;
    }
    let mut out = BytesMut::new();
    // FLV header: [key-frame(1)<<4 | codec 7, avc_packet_type=0, cts=0,0,0]
    out.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
    // AVCDecoderConfigurationRecord
    out.push(0x01); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFF); // 6 bits reserved(111111) | 2 bits lengthSizeMinusOne=3
    out.push(0xE1); // 3 bits reserved(111) | 5 bits numSPS=1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(0x01); // numPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Some(out.freeze())
}

/// Build an AVCC sample (FLV NALU layout) from an Annex-B frame.
fn build_avcc_sample(annexb: &[u8], key: bool) -> Option<Bytes> {
    let mut out = BytesMut::new();
    let frame_type: u8 = if key { 0x10 } else { 0x20 };
    out.push(frame_type | 7); // (frame_type<<4) | codec 7 (AVC)
    out.push(0x01); // avc_packet_type = 1 (NALU)
    out.extend_from_slice(&[0, 0, 0]); // composition time offset
    for (_, nalu) in split_nalus(annexb) {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(&nalu);
    }
    if out.len() <= 5 {
        None
    } else {
        Some(out.freeze())
    }
}
