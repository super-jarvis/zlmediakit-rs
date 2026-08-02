//! Shared WebRTC engine construction.
//!
//! Both WHEP (playback, we send media) and WHIP (publish, we receive media)
//! need the same codec set, but the "engineering" that makes WebRTC usable on
//! lossy networks differs by role:
//!
//! * *Sender* role (WHEP) installs the NACK responder (retransmit packets the
//!   remote reported lost), an SR generator, and the TWCC sender (stamps a
//!   transport-wide sequence number on every outgoing RTP packet so the remote
//!   peer can run congestion control / bandwidth estimation).
//! * *Receiver* role (WHIP) installs the NACK generator (request retransmission
//!   of packets we lost), an RR generator, and the TWCC receiver (sends
//!   transport-wide feedback back to the publisher so its bandwidth estimation
//!   can react to congestion).
//!
//! The interceptor `Registry` is attached to the `APIBuilder` and applied to
//! every `PeerConnection` created from it, transparently wiring the buffers and
//! retransmission logic around each negotiated RTP stream.

use anyhow::Result;
use ice::udp_network::UDPNetwork;
use std::net::IpAddr;
use webrtc::api::interceptor_registry::{
    configure_nack, configure_rtcp_reports, configure_twcc_receiver_only,
    configure_twcc_sender_only,
};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::interceptor::registry::Registry;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;

const H264_CAP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const OPUS_CAP: &str = "minptime=10;useinbandfec=1";

/// The role this peer plays in the RTP exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRtcRole {
    /// We send media (WHEP playback): retransmit on NACK, stamp TWCC on RTP.
    Sender,
    /// We receive media (WHIP publish): send NACK / TWCC feedback / RR.
    Receiver,
}

/// Process-wide ICE transport options shared by WHIP and WHEP sessions.
/// A cloned `UDPNetwork::Muxed` keeps every peer connection on the same UDP
/// socket instead of opening an unpredictable ephemeral port per session.
#[derive(Clone, Default)]
pub(crate) struct WebRtcTransportSettings {
    pub udp_network: Option<UDPNetwork>,
    pub bind_ip: Option<IpAddr>,
    pub ice_lite: bool,
    pub external_ips: Vec<String>,
}

/// Build an `APIBuilder` preloaded with the shared codec set and the
/// interceptor chain matching `role`.
pub fn build_api(role: WebRtcRole) -> Result<APIBuilder> {
    build_api_with_transport(role, &WebRtcTransportSettings::default())
}

pub(crate) fn build_api_with_transport(
    role: WebRtcRole,
    transport: &WebRtcTransportSettings,
) -> Result<APIBuilder> {
    let mut m = MediaEngine::default();
    m.register_codec(
        codec_params(
            webrtc::api::media_engine::MIME_TYPE_H264,
            90_000,
            0,
            H264_CAP,
            102,
        ),
        RTPCodecType::Video,
    )?;
    for (mime, payload_type) in [
        (webrtc::api::media_engine::MIME_TYPE_HEVC, 103),
        (webrtc::api::media_engine::MIME_TYPE_VP8, 96),
        (webrtc::api::media_engine::MIME_TYPE_VP9, 98),
    ] {
        if role == WebRtcRole::Receiver && mime == webrtc::api::media_engine::MIME_TYPE_HEVC {
            continue;
        }
        m.register_codec(
            codec_params(mime, 90_000, 0, "", payload_type),
            RTPCodecType::Video,
        )?;
    }
    if role == WebRtcRole::Sender {
        m.register_codec(
            codec_params(webrtc::api::media_engine::MIME_TYPE_AV1, 90_000, 0, "", 100),
            RTPCodecType::Video,
        )?;
    }
    let mut rtx_pairs = vec![(102, 101), (96, 97), (98, 99)];
    if role == WebRtcRole::Sender {
        rtx_pairs.extend([(103, 104), (100, 105)]);
    }
    for (associated_payload_type, rtx_payload_type) in rtx_pairs {
        m.register_codec(
            codec_params(
                "video/rtx",
                90_000,
                0,
                &format!("apt={associated_payload_type}"),
                rtx_payload_type,
            ),
            RTPCodecType::Video,
        )?;
    }
    m.register_codec(
        codec_params(
            webrtc::api::media_engine::MIME_TYPE_OPUS,
            48_000,
            2,
            OPUS_CAP,
            111,
        ),
        RTPCodecType::Audio,
    )?;

    // Interceptors must be configured *after* the codecs so their feedback
    // declarations land on every registered codec of the matching type.
    let mut registry = Registry::new();
    registry = configure_nack(registry, &mut m);
    registry = configure_rtcp_reports(registry);
    registry = match role {
        WebRtcRole::Sender => configure_twcc_sender_only(registry, &mut m)?,
        WebRtcRole::Receiver => configure_twcc_receiver_only(registry, &mut m)?,
    };

    let mut settings = SettingEngine::default();
    if let Some(udp_network) = &transport.udp_network {
        settings.set_udp_network(udp_network.clone());
    }
    if let Some(bind_ip) = transport.bind_ip {
        settings.set_ip_filter(Box::new(move |candidate| candidate == bind_ip));
        settings.set_include_loopback_candidate(bind_ip.is_loopback());
    }
    settings.set_lite(transport.ice_lite);
    if !transport.external_ips.is_empty() {
        settings.set_nat_1to1_ips(transport.external_ips.clone(), RTCIceCandidateType::Host);
    }
    if role == WebRtcRole::Sender {
        settings.enable_sender_rtx(true);
    }

    Ok(APIBuilder::new()
        .with_setting_engine(settings)
        .with_media_engine(m)
        .with_interceptor_registry(registry))
}

fn codec_params(
    mime: &str,
    clock: u32,
    channels: u16,
    fmtp: &str,
    pt: u8,
) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: mime.to_owned(),
            clock_rate: clock,
            channels,
            sdp_fmtp_line: fmtp.to_string(),
            rtcp_feedback: if mime.starts_with("video/") && mime != "video/rtx" {
                vec![RTCPFeedback {
                    typ: "goog-remb".to_string(),
                    parameter: String::new(),
                }]
            } else {
                vec![]
            },
        },
        payload_type: pt,
        stats_id: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

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

    #[tokio::test]
    async fn sender_engine_advertises_nack_and_twcc() {
        crate::init_crypto();
        let api = build_api(WebRtcRole::Sender).unwrap();
        let pc = api
            .build()
            .new_peer_connection(Default::default())
            .await
            .unwrap();
        let track = Arc::new(TrackLocalStaticSample::new(
            h264_cap(),
            "video".to_string(),
            "zlmediakit".to_string(),
        ));
        let _ = pc.add_track(track.clone()).await;
        let offer = pc.create_offer(None).await.unwrap();
        pc.set_local_description(offer).await.unwrap();
        let sdp = pc.local_description().await.unwrap().sdp;
        assert!(
            sdp.contains("a=rtcp-fb:102 nack\r\n"),
            "offer must advertise generic nack feedback:\n{sdp}"
        );
        assert!(
            sdp.contains("a=rtcp-fb:102 nack pli\r\n"),
            "offer must advertise pli feedback:\n{sdp}"
        );
        assert!(
            sdp.contains("transport-wide-cc"),
            "offer must advertise the transport-wide-cc header extension:\n{sdp}"
        );
        assert!(
            sdp.contains("a=rtcp-fb:102 goog-remb\r\n"),
            "offer must advertise REMB feedback:\n{sdp}"
        );
        for codec in ["HEVC/90000", "VP8/90000", "VP9/90000", "AV1/90000"] {
            assert!(
                sdp.contains(codec),
                "offer must advertise {codec} for extended video interoperability:\n{sdp}"
            );
        }
        for apt in ["apt=102", "apt=103", "apt=96", "apt=98", "apt=100"] {
            assert!(
                sdp.contains(apt),
                "offer must advertise RTX association {apt}:\n{sdp}"
            );
        }
        pc.close().await.unwrap();
    }

    #[tokio::test]
    async fn receiver_engine_advertises_nack_and_twcc_feedback() {
        crate::init_crypto();
        let api = build_api(WebRtcRole::Receiver).unwrap();
        let pc = api
            .build()
            .new_peer_connection(Default::default())
            .await
            .unwrap();
        let track = Arc::new(TrackLocalStaticSample::new(
            h264_cap(),
            "video".to_string(),
            "zlmediakit".to_string(),
        ));
        let _ = pc.add_track(track.clone()).await;
        let offer = pc.create_offer(None).await.unwrap();
        pc.set_local_description(offer).await.unwrap();
        let sdp = pc.local_description().await.unwrap().sdp;
        assert!(
            sdp.contains("a=rtcp-fb:102 nack\r\n"),
            "offer must advertise generic nack feedback:\n{sdp}"
        );
        assert!(
            sdp.contains("a=rtcp-fb:102 transport-cc\r\n"),
            "offer must advertise transport-cc feedback:\n{sdp}"
        );
        pc.close().await.unwrap();
    }
}
