//! Native WHIP push client used by stream pushers.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::media_frame::{CodecId, MediaFrame, TrackInfo};
use zlmediakit_core::media_source::MediaSourceManager;

use crate::engine::{WebRtcRole, WebRtcTransportSettings};
use crate::pull_client::{request, SignallingUrl};
use crate::whep::{build_rtc_config, opus_capability, video_capability, video_sample};

type LocalTrack = (u32, CodecId, Arc<TrackLocalStaticSample>);

async fn wait_for_tracks(
    source: &zlmediakit_core::media_source::MediaSource,
    stopped: &AtomicBool,
) -> Result<Vec<TrackInfo>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if stopped.load(Ordering::SeqCst) {
            bail!("WHIP push stopped before source tracks became available");
        }
        let tracks = source.info.read().await.tracks.clone();
        if !tracks.is_empty() {
            return Ok(tracks);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("WHIP push source has no tracks");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn write_frame(
    frame: &MediaFrame,
    video_tracks: &[LocalTrack],
    audio_tracks: &[LocalTrack],
    base_time: SystemTime,
) {
    let timestamp = base_time + Duration::from_millis(frame.dts);
    match frame.codec {
        CodecId::H264 | CodecId::H265 | CodecId::VP8 | CodecId::VP9 | CodecId::AV1 => {
            let Some((_, _, track)) = video_tracks
                .iter()
                .find(|(track_id, codec, _)| *track_id == frame.track_id && *codec == frame.codec)
            else {
                return;
            };
            let Some(data) = video_sample(frame) else {
                return;
            };
            if let Err(error) = track
                .write_sample(&Sample {
                    data,
                    timestamp,
                    duration: Duration::from_millis(40),
                    ..Default::default()
                })
                .await
            {
                debug!("WHIP video write_sample failed: {error}");
            }
        }
        CodecId::Opus => {
            let Some((_, _, track)) = audio_tracks
                .iter()
                .find(|(track_id, _, _)| *track_id == frame.track_id)
            else {
                return;
            };
            if let Err(error) = track
                .write_sample(&Sample {
                    data: frame.data.clone(),
                    timestamp,
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await
            {
                debug!("WHIP audio write_sample failed: {error}");
            }
        }
        _ => {}
    }
}

/// Push a local media source to a remote WHIP resource.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    ice_servers: Vec<IceServer>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    crate::init_crypto();
    let target = SignallingUrl::parse(url)?.as_http_url();
    let source = source_manager
        .get(vhost, app, stream_name)
        .ok_or_else(|| anyhow!("WHIP push source not found: {vhost}/{app}/{stream_name}"))?;
    let source_tracks = wait_for_tracks(&source, &stopped).await?;
    info!("WHIP push: {vhost}/{app}/{stream_name} -> {target}");

    let api = crate::engine::build_api_with_transport(
        WebRtcRole::Sender,
        &WebRtcTransportSettings::default(),
    )?;
    let pc = Arc::new(
        api.build()
            .new_peer_connection(build_rtc_config(&ice_servers))
            .await?,
    );
    let mut video_tracks = Vec::new();
    let mut audio_tracks = Vec::new();
    for (track_id, track_info) in source_tracks.iter().enumerate() {
        let (codec, capability, output) = match track_info {
            TrackInfo::Video(video) => {
                let Some(capability) = video_capability(video.codec) else {
                    continue;
                };
                (video.codec, capability, &mut video_tracks)
            }
            TrackInfo::Audio(audio) if audio.codec == CodecId::Opus => {
                (audio.codec, opus_capability(), &mut audio_tracks)
            }
            _ => continue,
        };
        let track = Arc::new(TrackLocalStaticSample::new(
            capability,
            format!("track-{track_id}"),
            "zlmediakit".to_string(),
        ));
        let sender = pc.add_track(track.clone()).await?;
        tokio::spawn(async move { while sender.read_rtcp().await.is_ok() {} });
        output.push((track_id as u32, codec, track));
    }
    if video_tracks.is_empty() && audio_tracks.is_empty() {
        bail!("WHIP push source has no WebRTC-compatible tracks");
    }

    let offer = pc.create_offer(None).await?;
    let mut gathering_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    tokio::time::timeout(Duration::from_secs(10), gathering_complete.recv())
        .await
        .context("WHIP ICE candidate gathering timed out")?;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("WHIP push has no local description"))?
        .sdp;

    let response = request("POST", &target, offer_sdp.as_bytes()).await?;
    if response.status != 201 {
        bail!(
            "WHIP upstream returned status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        );
    }
    let location = response
        .headers
        .get("location")
        .ok_or_else(|| anyhow!("WHIP response is missing Location"))?;
    let resource_url = response.url.resolve(location)?;
    let answer_sdp = String::from_utf8(response.body).context("WHIP answer is not UTF-8 SDP")?;
    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;
    info!("WHIP push negotiated: {resource_url}");

    let mut receiver = source.subscribe();
    let base_time = SystemTime::now();
    for frame in source.get_latest_gop_frames().await {
        write_frame(&frame, &video_tracks, &audio_tracks, base_time).await;
    }
    loop {
        if stopped.load(Ordering::SeqCst)
            || matches!(
                pc.connection_state(),
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            )
        {
            break;
        }
        tokio::select! {
            _ = stop.notified() => {}
            result = receiver.recv() => match result {
                Ok(frame) => write_frame(&frame, &video_tracks, &audio_tracks, base_time).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }

    match tokio::time::timeout(
        Duration::from_secs(5),
        request("DELETE", &resource_url, &[]),
    )
    .await
    {
        Ok(Ok(response)) if (200..300).contains(&response.status) => {
            debug!(status = response.status, "WHIP upstream resource deleted");
        }
        Ok(Ok(response)) => warn!(status = response.status, "WHIP DELETE was rejected"),
        Ok(Err(error)) => warn!("WHIP DELETE failed: {error}"),
        Err(_) => warn!("WHIP DELETE timed out"),
    }
    pc.close().await?;
    info!("WHIP push finished for {target}");
    Ok(())
}
