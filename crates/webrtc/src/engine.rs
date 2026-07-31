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
use webrtc::api::interceptor_registry::{
    configure_nack, configure_rtcp_reports, configure_twcc_receiver_only,
    configure_twcc_sender_only,
};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};

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

/// Build an `APIBuilder` preloaded with the shared codec set and the
/// interceptor chain matching `role`.
pub fn build_api(role: WebRtcRole) -> Result<APIBuilder> {
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

    Ok(APIBuilder::new()
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
            rtcp_feedback: vec![],
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
