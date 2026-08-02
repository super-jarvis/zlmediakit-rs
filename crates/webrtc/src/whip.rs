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
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{debug, info, warn};
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::RTCRtpTransceiver;
use webrtc::track::track_remote::TrackRemote;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::event_bus::Event;
use zlmediakit_core::media_frame::{
    AudioInfo, CodecId, MediaFrame, PayloadFormat, TrackInfo, VideoInfo,
};
use zlmediakit_core::media_source::{MediaSource, TrackDescriptor};

use crate::datachannel::{attach_data_channels, DataMessage};
use crate::h264_rtp::H264RtpDepacketizer;
use crate::whep::build_rtc_config;

/// A live WHIP session. Held by the HTTP server until the client issues a
/// DELETE or the server shuts down.
pub struct WhipSession {
    pub pc: Arc<RTCPeerConnection>,
    pub resource: String,
    data_tx: broadcast::Sender<DataMessage>,
    source: Arc<MediaSource>,
    closed: AtomicBool,
    negotiation: Arc<Mutex<()>>,
}

impl WhipSession {
    /// Subscribe to messages arriving over this session's WebRTC DataChannels.
    pub fn subscribe_data(&self) -> broadcast::Receiver<DataMessage> {
        self.data_tx.subscribe()
    }

    /// Tear down the PeerConnection.
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(event_bus) = &self.source.event_bus {
            event_bus.publish(Event::StreamUnPublish {
                source_id: self.source.id.clone(),
                vhost: self.source.vhost.clone(),
                app: self.source.app.clone(),
                stream: self.source.stream.clone(),
            });
        }
        self.source.close();
        if let Err(e) = self.pc.close().await {
            warn!("webrtc: error closing whip pc for {}: {}", self.resource, e);
        }
    }

    pub fn source_identity(&self) -> (String, String, String) {
        (
            self.source.vhost.clone(),
            self.source.app.clone(),
            self.source.stream.clone(),
        )
    }

    pub(crate) fn negotiation_lock(&self) -> Arc<Mutex<()>> {
        self.negotiation.clone()
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
    let transport = crate::engine::WebRtcTransportSettings::default();
    whip_publish_with_transport(offer_sdp, source, resource, ice_servers, &transport).await
}

pub(crate) async fn whip_publish_with_transport(
    offer_sdp: &str,
    source: Arc<MediaSource>,
    resource: String,
    ice_servers: Vec<IceServer>,
    transport: &crate::engine::WebRtcTransportSettings,
) -> Result<(String, WhipSession)> {
    crate::init_crypto();
    let api =
        crate::engine::build_api_with_transport(crate::engine::WebRtcRole::Receiver, transport)?;
    let cfg = build_rtc_config(&ice_servers);
    let pc = Arc::new(api.build().new_peer_connection(cfg).await?);

    // DataChannel plumbing: any channel the publisher opens is forwarded into a
    // broadcast pipe the HTTP layer (or a control plane) can subscribe to.
    let (data_tx, _) = broadcast::channel(64);
    attach_data_channels(&pc, data_tx.clone());

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

    attach_media_receiver(&pc, source.clone());

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

    if let Some(event_bus) = &source.event_bus {
        event_bus.publish(Event::StreamPublish {
            source_id: source.id.clone(),
            vhost: source.vhost.clone(),
            app: source.app.clone(),
            stream: source.stream.clone(),
        });
    }

    info!("webrtc: WHIP session {} negotiated", resource);
    Ok((
        answer_sdp,
        WhipSession {
            pc,
            resource,
            data_tx,
            source,
            closed: AtomicBool::new(false),
            negotiation: Arc::new(Mutex::new(())),
        },
    ))
}

/// Attach the shared remote-track -> `MediaSource` receive pipeline to a peer.
///
/// Both a WHIP server and a native WHEP pull client receive RTP from a remote
/// peer. Keeping the callback here guarantees that codec registration, track
/// descriptors and frame conversion stay identical for both ingress paths.
pub(crate) fn attach_media_receiver(pc: &Arc<RTCPeerConnection>, source: Arc<MediaSource>) {
    let src = source;
    let next_track_id = Arc::new(tokio::sync::Mutex::new(0u32));
    pc.on_track(Box::new(
        move |track: Arc<TrackRemote>,
              _recv: Arc<RTCRtpReceiver>,
              trans: Arc<RTCRtpTransceiver>| {
            let src = src.clone();
            let next_track_id = next_track_id.clone();
            let kind = trans.kind();
            Box::pin(async move {
                let mime = track.codec().capability.mime_type;
                let Some(codec) = codec_from_mime(&mime) else {
                    warn!("webrtc: WHIP ignoring unsupported codec {mime}");
                    return;
                };
                let descriptor = TrackDescriptor {
                    rid: (!track.rid().is_empty()).then(|| track.rid().to_owned()),
                    remote_track_id: Some(track.id()),
                    stream_id: Some(track.stream_id()),
                    ssrc: Some(track.ssrc()),
                };
                let track_info = match kind {
                    RTPCodecType::Video => TrackInfo::Video(VideoInfo {
                        codec,
                        width: 0,
                        height: 0,
                        fps: 0.0,
                        key_frame: false,
                    }),
                    RTPCodecType::Audio => TrackInfo::Audio(AudioInfo {
                        codec,
                        sample_rate: 48_000,
                        channels: 2,
                        bits_per_sample: 16,
                    }),
                    _ => return,
                };
                // Keep id allocation and MediaInfo insertion in the same
                // critical section. `on_track` callbacks run concurrently, so
                // releasing this lock before the async write could reorder the
                // vector and make its index disagree with the frame track id.
                let track_id = {
                    let mut next = next_track_id.lock().await;
                    let track_id = *next;
                    src.info.write().await.tracks.push(track_info);
                    *next += 1;
                    track_id
                };
                src.set_track_descriptor(track_id, descriptor).await;
                match kind {
                    RTPCodecType::Video => receive_video(track, src, track_id, codec).await,
                    RTPCodecType::Audio => receive_audio(track, src, track_id).await,
                    _ => debug!("webrtc: whip ignoring track of unknown kind"),
                }
            })
        },
    ));
}

async fn receive_video(
    track: Arc<TrackRemote>,
    source: Arc<MediaSource>,
    track_id: u32,
    codec: CodecId,
) {
    match codec {
        CodecId::H264 => receive_h264(track, source, track_id).await,
        CodecId::VP8 => {
            receive_sampled_video(
                track,
                source,
                track_id,
                codec,
                webrtc::rtp::codecs::vp8::Vp8Packet::default(),
            )
            .await
        }
        CodecId::VP9 => {
            receive_sampled_video(
                track,
                source,
                track_id,
                codec,
                webrtc::rtp::codecs::vp9::Vp9Packet::default(),
            )
            .await
        }
        _ => warn!("webrtc: no WHIP video depacketizer for {codec:?}"),
    }
}

async fn receive_h264(track: Arc<TrackRemote>, source: Arc<MediaSource>, track_id: u32) {
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
        // RTP timestamp uses a 90 kHz clock for H.264; convert to milliseconds.
        let ts_ms = (pkt.header.timestamp as u64) / 90;
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
                    let mut f = MediaFrame::new_video(
                        track_id,
                        CodecId::H264,
                        ts_ms as u32,
                        ts_ms,
                        ts_ms,
                        cfg,
                        true,
                    )
                    .with_payload_format(PayloadFormat::Flv);
                    f.config_frame = true;
                    source.publish_and_cache(f).await;
                    config_sent = true;
                }
            }
        } else {
            // VCL frame: ensure the decoder config was sent first.
            if !config_sent {
                if let Some(cfg) = build_avcc_config(sps.as_deref(), pps.as_deref()) {
                    let mut f = MediaFrame::new_video(
                        track_id,
                        CodecId::H264,
                        ts_ms as u32,
                        ts_ms,
                        ts_ms,
                        cfg,
                        true,
                    )
                    .with_payload_format(PayloadFormat::Flv);
                    f.config_frame = true;
                    source.publish_and_cache(f).await;
                    config_sent = true;
                }
            }
            if let Some(sample) = build_avcc_sample(&annexb, key) {
                source
                    .publish_and_cache(
                        MediaFrame::new_video(
                            track_id,
                            CodecId::H264,
                            ts_ms as u32,
                            ts_ms,
                            ts_ms,
                            sample,
                            key,
                        )
                        .with_payload_format(PayloadFormat::Flv),
                    )
                    .await;
            }
        }
    }
    info!("webrtc: whip video receiver ended");
}

async fn receive_sampled_video<T>(
    track: Arc<TrackRemote>,
    source: Arc<MediaSource>,
    track_id: u32,
    codec: CodecId,
    depacketizer: T,
) where
    T: Depacketizer + Send,
{
    let mut builder = SampleBuilder::new(64, depacketizer, 90_000);
    loop {
        let packet = match track.read_rtp().await {
            Ok((packet, _)) => packet,
            Err(_) => break,
        };
        builder.push(packet);
        while let Some((sample, rtp_timestamp)) = builder.pop_with_timestamp() {
            let timestamp_ms = (rtp_timestamp as u64) / 90;
            let key_frame = match codec {
                CodecId::VP8 => sample.data.first().is_some_and(|byte| byte & 0x01 == 0),
                CodecId::VP9 => vp9_is_key_frame(&sample.data),
                _ => false,
            };
            source
                .publish_and_cache(MediaFrame::new_video(
                    track_id,
                    codec,
                    timestamp_ms as u32,
                    timestamp_ms,
                    timestamp_ms,
                    sample.data,
                    key_frame,
                ))
                .await;
        }
    }
    info!("webrtc: whip {codec:?} receiver ended");
}

fn codec_from_mime(mime: &str) -> Option<CodecId> {
    if mime.eq_ignore_ascii_case(webrtc::api::media_engine::MIME_TYPE_H264) {
        Some(CodecId::H264)
    } else if mime.eq_ignore_ascii_case(webrtc::api::media_engine::MIME_TYPE_VP8) {
        Some(CodecId::VP8)
    } else if mime.eq_ignore_ascii_case(webrtc::api::media_engine::MIME_TYPE_VP9) {
        Some(CodecId::VP9)
    } else if mime.eq_ignore_ascii_case(webrtc::api::media_engine::MIME_TYPE_OPUS) {
        Some(CodecId::Opus)
    } else {
        None
    }
}

fn vp9_is_key_frame(data: &[u8]) -> bool {
    let Some(first) = data.first().copied() else {
        return false;
    };
    if first & 0x03 != 0x02 {
        return false;
    }
    let profile = (first >> 2) & 0x03;
    let show_existing_bit = if profile == 3 { 5 } else { 4 };
    if (first >> show_existing_bit) & 0x01 != 0 {
        return false;
    }
    (first >> (show_existing_bit + 1)) & 0x01 == 0
}

async fn receive_audio(track: Arc<TrackRemote>, source: Arc<MediaSource>, track_id: u32) {
    let mut buf = vec![0u8; 4096];
    loop {
        let pkt = match track.read(&mut buf).await {
            Ok((p, _)) => p,
            Err(_) => break,
        };
        // Opus RTP uses a 48 kHz clock; convert to milliseconds.
        let ts_ms = (pkt.header.timestamp as u64) / 48;
        // Opus RTP payload is the raw Opus frame(s); forward as-is so a WHEP
        // player can repacketize it.
        source
            .publish_and_cache(
                MediaFrame::new_audio(
                    track_id,
                    CodecId::Opus,
                    ts_ms as u32,
                    ts_ms,
                    ts_ms,
                    Bytes::copy_from_slice(&pkt.payload),
                )
                .with_payload_format(PayloadFormat::Opus),
            )
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
        if i + 4 <= n && annexb[i..i + 4] == [0, 0, 0, 1] {
            i += 4;
        } else if i + 3 <= n && annexb[i..i + 3] == [0, 0, 1] {
            i += 3;
        } else {
            i += 1;
            continue;
        }
        let start = i;
        while i < n {
            if i + 4 <= n && annexb[i..i + 4] == [0, 0, 0, 1] {
                break;
            }
            if i + 3 <= n && annexb[i..i + 3] == [0, 0, 1] {
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
    out.put_u8(0x01); // configurationVersion
    out.put_u8(sps[1]); // AVCProfileIndication
    out.put_u8(sps[2]); // profile_compatibility
    out.put_u8(sps[3]); // AVCLevelIndication
    out.put_u8(0xFF); // 6 bits reserved(111111) | 2 bits lengthSizeMinusOne=3
    out.put_u8(0xE1); // 3 bits reserved(111) | 5 bits numSPS=1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.put_u8(0x01); // numPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Some(out.freeze())
}

/// Build an AVCC sample (FLV NALU layout) from an Annex-B frame.
fn build_avcc_sample(annexb: &[u8], key: bool) -> Option<Bytes> {
    let mut out = BytesMut::new();
    let frame_type: u8 = if key { 0x10 } else { 0x20 };
    out.put_u8(frame_type | 7); // (frame_type<<4) | codec 7 (AVC)
    out.put_u8(0x01); // avc_packet_type = 1 (NALU)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_whip_codecs_case_insensitively() {
        assert_eq!(codec_from_mime("video/H264"), Some(CodecId::H264));
        assert_eq!(codec_from_mime("VIDEO/VP8"), Some(CodecId::VP8));
        assert_eq!(codec_from_mime("video/VP9"), Some(CodecId::VP9));
        assert_eq!(codec_from_mime("audio/opus"), Some(CodecId::Opus));
        assert_eq!(codec_from_mime("video/H265"), None);
        assert_eq!(codec_from_mime("video/AV1"), None);
    }

    #[test]
    fn detects_vp9_key_frame_from_uncompressed_header() {
        // Frame marker 0b10, profile 0, show_existing_frame=0,
        // frame_type=0 (key frame).
        assert!(vp9_is_key_frame(&[0b0000_0010]));
        // Same header with frame_type=1 (inter frame).
        assert!(!vp9_is_key_frame(&[0b0010_0010]));
        // show_existing_frame=1 cannot be a new key frame.
        assert!(!vp9_is_key_frame(&[0b0001_0010]));
        assert!(!vp9_is_key_frame(&[]));
        assert!(!vp9_is_key_frame(&[0]));
    }
}
