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
//!   streams but is not a WebRTC codec. With the `transcode` Cargo feature
//!   (and a working AAC decoder) an AAC source is transcoded to Opus on the
//!   fly; without it the AAC track is silently skipped.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{debug, info, warn};
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo};
use zlmediakit_core::media_source::{MediaSource, SubscriberGuard, TrackDescriptor};
#[cfg(feature = "transcode")]
use zlmediakit_core::{aac_audio_specific_config, aac_raw_payload};
use zlmediakit_core::{video_config_annex_b, video_sample_annex_b};

use crate::datachannel::{attach_data_channels, DataMessage};

#[cfg(feature = "transcode")]
use crate::transcode::AudioTranscoder;

/// A live WHEP session. Held by the HTTP server so the `PeerConnection` (and the
/// background media pump) stay alive until the client issues a DELETE or the
/// server shuts down.
pub struct WhepSession {
    pub pc: Arc<RTCPeerConnection>,
    pub resource: String,
    data_tx: broadcast::Sender<DataMessage>,
    stop: Arc<Notify>,
    subscriber: Mutex<Option<SubscriberGuard>>,
    estimated_bitrate_bps: Arc<AtomicU64>,
    negotiation: Arc<Mutex<()>>,
}

type LocalTrack = (u32, CodecId, Arc<TrackLocalStaticSample>);

fn track_matches_rid(descriptor: Option<&TrackDescriptor>, requested_rid: Option<&str>) -> bool {
    requested_rid.is_none()
        || descriptor.and_then(|descriptor| descriptor.rid.as_deref()) == requested_rid
}

pub(crate) fn video_capability(codec: CodecId) -> Option<RTCRtpCodecCapability> {
    let (mime_type, sdp_fmtp_line) = match codec {
        CodecId::H264 => (
            webrtc::api::media_engine::MIME_TYPE_H264,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        ),
        CodecId::H265 => (webrtc::api::media_engine::MIME_TYPE_HEVC, ""),
        CodecId::VP8 => (webrtc::api::media_engine::MIME_TYPE_VP8, ""),
        CodecId::VP9 => (webrtc::api::media_engine::MIME_TYPE_VP9, ""),
        CodecId::AV1 => (webrtc::api::media_engine::MIME_TYPE_AV1, ""),
        _ => return None,
    };
    Some(RTCRtpCodecCapability {
        mime_type: mime_type.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: sdp_fmtp_line.to_string(),
        rtcp_feedback: vec![],
    })
}

pub(crate) fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: webrtc::api::media_engine::MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
        rtcp_feedback: vec![],
    }
}

impl WhepSession {
    /// Subscribe to messages arriving over this session's WebRTC DataChannels.
    pub fn subscribe_data(&self) -> broadcast::Receiver<DataMessage> {
        self.data_tx.subscribe()
    }

    /// Tear down the PeerConnection and stop the media pump.
    pub async fn close(&self) {
        self.stop.notify_one();
        if let Some(subscriber) = self.subscriber.lock().await.take() {
            subscriber.close().await;
        }
        if let Err(e) = self.pc.close().await {
            warn!("webrtc: error closing pc for {}: {}", self.resource, e);
        }
    }

    /// Latest receiver-estimated maximum bitrate reported through RTCP REMB.
    /// Zero means the player has not sent a REMB packet yet.
    pub fn estimated_bitrate_bps(&self) -> u64 {
        self.estimated_bitrate_bps.load(Ordering::Relaxed)
    }

    pub(crate) fn negotiation_lock(&self) -> Arc<Mutex<()>> {
        self.negotiation.clone()
    }
}

fn spawn_rtcp_observer(sender: Arc<RTCRtpSender>, estimated_bitrate_bps: Arc<AtomicU64>) {
    tokio::spawn(async move {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            for packet in packets {
                let Some(remb) = packet
                    .as_any()
                    .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                else {
                    continue;
                };
                if remb.bitrate.is_finite() && remb.bitrate > 0.0 {
                    let bitrate = remb.bitrate as u64;
                    estimated_bitrate_bps.store(bitrate, Ordering::Relaxed);
                    debug!("webrtc: received REMB estimate {bitrate} bit/s");
                }
            }
        }
    });
}

/// Negotiate a WHEP playback session.
///
/// * `offer_sdp` — the SDP offer posted by the client.
/// * `source`    — the local stream to play.
/// * `resource`  — opaque id the HTTP layer uses for DELETE / bookkeeping.
///
/// Returns the SDP answer (to be returned to the client with the proper
/// `Content-Type` / `Location` headers) and the kept-alive session.
pub async fn whep_play(
    offer_sdp: &str,
    source: Arc<MediaSource>,
    resource: String,
    ice_servers: Vec<IceServer>,
) -> Result<(String, WhepSession)> {
    let transport = crate::engine::WebRtcTransportSettings::default();
    whep_play_with_transport(offer_sdp, source, resource, ice_servers, &transport, None).await
}

pub(crate) async fn whep_play_with_transport(
    offer_sdp: &str,
    source: Arc<MediaSource>,
    resource: String,
    ice_servers: Vec<IceServer>,
    transport: &crate::engine::WebRtcTransportSettings,
    requested_rid: Option<&str>,
) -> Result<(String, WhepSession)> {
    crate::init_crypto();
    let api =
        crate::engine::build_api_with_transport(crate::engine::WebRtcRole::Sender, transport)?;
    let pc = Arc::new(
        api.build()
            .new_peer_connection(build_rtc_config(&ice_servers))
            .await?,
    );

    // DataChannel plumbing: any channel the player opens is forwarded into a
    // broadcast pipe the HTTP layer (or a control plane) can subscribe to.
    let (data_tx, _) = broadcast::channel(64);
    attach_data_channels(&pc, data_tx.clone());

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

    let source_tracks = source.info.read().await.tracks.clone();
    let descriptors = source.track_descriptors.read().await.clone();
    let estimated_bitrate_bps = Arc::new(AtomicU64::new(0));
    let mut video_tracks = Vec::<LocalTrack>::new();
    let mut audio_tracks = Vec::<LocalTrack>::new();
    for (track_id, track_info) in source_tracks.iter().enumerate() {
        match track_info {
            TrackInfo::Video(video) => {
                if let Some(requested_rid) = requested_rid {
                    if !track_matches_rid(descriptors.get(&(track_id as u32)), Some(requested_rid))
                    {
                        continue;
                    }
                }
                let Some(capability) = video_capability(video.codec) else {
                    continue;
                };
                let track = Arc::new(TrackLocalStaticSample::new(
                    capability,
                    format!("video-{track_id}"),
                    "zlmediakit".to_string(),
                ));
                let sender = pc.add_track(track.clone()).await?;
                spawn_rtcp_observer(sender, estimated_bitrate_bps.clone());
                video_tracks.push((track_id as u32, video.codec, track));
            }
            TrackInfo::Audio(audio) if audio.codec == CodecId::Opus => {
                let track = Arc::new(TrackLocalStaticSample::new(
                    opus_capability(),
                    format!("audio-{track_id}"),
                    "zlmediakit".to_string(),
                ));
                let sender = pc.add_track(track.clone()).await?;
                spawn_rtcp_observer(sender, estimated_bitrate_bps.clone());
                audio_tracks.push((track_id as u32, audio.codec, track));
            }
            _ => {}
        }
    }
    if requested_rid.is_some() && video_tracks.is_empty() {
        return Err(anyhow!(
            "webrtc: requested simulcast RID {:?} is not available",
            requested_rid
        ));
    }

    // Optional AAC -> Opus transcoding so an AAC-only source can still be played
    // over WebRTC. Only compiled with the `transcode` feature.
    #[cfg(feature = "transcode")]
    let mut transcoder: Option<Arc<tokio::sync::Mutex<AudioTranscoder>>> = None;
    #[cfg(feature = "transcode")]
    if audio_tracks.is_empty() {
        let aac_track =
            source_tracks
                .iter()
                .enumerate()
                .find_map(|(track_id, track)| match track {
                    TrackInfo::Audio(audio) if audio.codec == CodecId::AAC => {
                        Some((track_id as u32, audio.sample_rate, audio.channels as u8))
                    }
                    _ => None,
                });
        if let Some((track_id, rate, channels)) = aac_track {
            let track = Arc::new(TrackLocalStaticSample::new(
                opus_capability(),
                format!("audio-{track_id}"),
                "zlmediakit".to_string(),
            ));
            let sender = pc.add_track(track.clone()).await?;
            spawn_rtcp_observer(sender, estimated_bitrate_bps.clone());
            audio_tracks.push((track_id, CodecId::AAC, track));

            #[cfg(feature = "aac-transcode")]
            let decoder: Option<Box<dyn crate::transcode::Decoder>> = {
                match crate::transcode::FdkAacDecoder::new(rate, channels) {
                    Ok(d) => Some(Box::new(d)),
                    Err(_) => None,
                }
            };
            #[cfg(not(feature = "aac-transcode"))]
            let decoder: Option<Box<dyn crate::transcode::Decoder>> = None;

            transcoder = Some(Arc::new(tokio::sync::Mutex::new(AudioTranscoder::new(
                decoder, 48_000, 2,
            )?)));
        }
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
    let subscriber = source
        .register_subscriber(format!("webrtc:{resource}"))
        .await;
    let stop = Arc::new(Notify::new());

    #[cfg(feature = "transcode")]
    spawn_pump(source, video_tracks, audio_tracks, transcoder, stop.clone());
    #[cfg(not(feature = "transcode"))]
    spawn_pump(source, video_tracks, audio_tracks, stop.clone());

    info!("webrtc: WHEP session {} negotiated", resource);
    Ok((
        answer_sdp,
        WhepSession {
            pc,
            resource,
            data_tx,
            stop,
            subscriber: Mutex::new(Some(subscriber)),
            estimated_bitrate_bps,
            negotiation: Arc::new(Mutex::new(())),
        },
    ))
}

#[cfg(not(feature = "transcode"))]
fn spawn_pump(
    source: Arc<MediaSource>,
    video: Vec<LocalTrack>,
    audio: Vec<LocalTrack>,
    stop: Arc<Notify>,
) {
    tokio::spawn(async move {
        let base_time = SystemTime::now();
        let cached = source.get_latest_gop_frames().await;
        for f in &cached {
            push_frame(f, &video, &audio, base_time).await;
        }
        let mut rx = source.subscribe();
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                result = rx.recv() => match result {
                    Ok(f) => push_frame(&f, &video, &audio, base_time).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        info!("webrtc: media pump ended");
    });
}

#[cfg(not(feature = "transcode"))]
async fn push_frame(
    f: &MediaFrame,
    video: &[LocalTrack],
    audio: &[LocalTrack],
    base_time: SystemTime,
) {
    let ts = base_time + Duration::from_millis(f.dts);
    match f.codec {
        CodecId::H264 | CodecId::H265 | CodecId::VP8 | CodecId::VP9 | CodecId::AV1 => {
            if let Some(v) = select_local_track(video, f) {
                let Some(data) = video_sample(f) else {
                    return;
                };
                let sample = webrtc::media::Sample {
                    data,
                    timestamp: ts,
                    duration: Duration::from_millis(40),
                    ..Default::default()
                };
                if let Err(e) = v.write_sample(&sample).await {
                    debug!("webrtc: video write_sample error: {}", e);
                }
            }
        }
        CodecId::Opus => {
            if let Some(a) = select_local_track(audio, f) {
                let sample = webrtc::media::Sample {
                    data: f.data.clone(),
                    timestamp: ts,
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

#[cfg(feature = "transcode")]
fn spawn_pump(
    source: Arc<MediaSource>,
    video: Vec<LocalTrack>,
    audio: Vec<LocalTrack>,
    transcoder: Option<Arc<tokio::sync::Mutex<AudioTranscoder>>>,
    stop: Arc<Notify>,
) {
    tokio::spawn(async move {
        let base_time = SystemTime::now();
        let cached = source.get_latest_gop_frames().await;
        for f in &cached {
            push_frame(f, &video, &audio, &transcoder, base_time).await;
        }
        let mut rx = source.subscribe();
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                result = rx.recv() => match result {
                    Ok(f) => push_frame(&f, &video, &audio, &transcoder, base_time).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        info!("webrtc: media pump ended");
    });
}

#[cfg(feature = "transcode")]
async fn push_frame(
    f: &MediaFrame,
    video: &[LocalTrack],
    audio: &[LocalTrack],
    transcoder: &Option<Arc<tokio::sync::Mutex<AudioTranscoder>>>,
    base_time: SystemTime,
) {
    let ts = base_time + Duration::from_millis(f.dts);
    match f.codec {
        CodecId::H264 | CodecId::H265 | CodecId::VP8 | CodecId::VP9 | CodecId::AV1 => {
            if let Some(v) = select_local_track(video, f) {
                let Some(data) = video_sample(f) else {
                    return;
                };
                let sample = webrtc::media::Sample {
                    data,
                    timestamp: ts,
                    duration: Duration::from_millis(40),
                    ..Default::default()
                };
                if let Err(e) = v.write_sample(&sample).await {
                    debug!("webrtc: video write_sample error: {}", e);
                }
            }
        }
        CodecId::Opus => {
            if let Some(a) = select_local_track(audio, f) {
                let sample = webrtc::media::Sample {
                    data: f.data.clone(),
                    timestamp: ts,
                    duration: Duration::from_millis(20),
                    ..Default::default()
                };
                if let Err(e) = a.write_sample(&sample).await {
                    debug!("webrtc: audio write_sample error: {}", e);
                }
            }
        }
        CodecId::AAC => {
            // Transcode AAC -> Opus so WebRTC can carry it.
            if let Some(t) = transcoder {
                let mut t = t.lock().await;
                if f.config_frame {
                    // First AAC frame carries the AudioSpecificConfig.
                    match aac_audio_specific_config(f) {
                        Ok(config) => {
                            if let Err(e) = t.configure(&config) {
                                debug!("webrtc: aac decoder config error: {}", e);
                            }
                        }
                        Err(e) => debug!("webrtc: invalid aac config frame: {}", e),
                    }
                } else if let Some(a) = select_local_track(audio, f) {
                    let Ok(raw) = aac_raw_payload(f) else {
                        return;
                    };
                    match t.transcode(&raw) {
                        Ok(packets) => {
                            for opus_bytes in packets {
                                let sample = webrtc::media::Sample {
                                    data: Bytes::from(opus_bytes),
                                    timestamp: ts,
                                    duration: Duration::from_millis(20),
                                    ..Default::default()
                                };
                                if let Err(e) = a.write_sample(&sample).await {
                                    debug!("webrtc: transcoded audio write_sample error: {}", e);
                                }
                            }
                        }
                        Err(e) => debug!("webrtc: transcode error: {}", e),
                    }
                }
            }
        }
        _ => {}
    }
}

fn select_local_track<'a>(
    tracks: &'a [LocalTrack],
    frame: &MediaFrame,
) -> Option<&'a Arc<TrackLocalStaticSample>> {
    if let Some((_, _, track)) = tracks
        .iter()
        .find(|(track_id, codec, _)| *track_id == frame.track_id && *codec == frame.codec)
    {
        return Some(track);
    }
    let mut matching = tracks.iter().filter(|(_, codec, _)| *codec == frame.codec);
    let (_, _, track) = matching.next()?;
    matching.next().is_none().then_some(track)
}

pub(crate) fn video_sample(frame: &MediaFrame) -> Option<Bytes> {
    match frame.codec {
        CodecId::H264 | CodecId::H265 => {
            if frame.config_frame {
                video_config_annex_b(frame).ok()
            } else {
                video_sample_annex_b(frame).ok()
            }
        }
        CodecId::VP8 | CodecId::VP9 | CodecId::AV1 => Some(frame.data.clone()),
        _ => None,
    }
}

impl Drop for WhepSession {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

/// Build a `webrtc` `RTCConfiguration` from the server's `IceServer` list so
/// STUN/TURN servers for NAT traversal can be configured via `conf/config.toml`.
/// Shared by both WHEP (playback) and WHIP (publish) negotiation.
pub(crate) fn build_rtc_config(ice_servers: &[IceServer]) -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod rid_tests {
    use super::*;

    #[test]
    fn selects_only_the_requested_simulcast_rid() {
        let high = TrackDescriptor {
            rid: Some("h".to_string()),
            ..Default::default()
        };
        let low = TrackDescriptor {
            rid: Some("l".to_string()),
            ..Default::default()
        };
        assert!(track_matches_rid(Some(&high), None));
        assert!(track_matches_rid(Some(&high), Some("h")));
        assert!(!track_matches_rid(Some(&low), Some("h")));
        assert!(!track_matches_rid(None, Some("h")));
    }
}
