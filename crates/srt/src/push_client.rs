use crate::client::{SrtEndpoint, SrtSocket};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::info;
use zlmediakit_core::{CodecId, MediaFrame, MediaSourceManager, TrackInfo};
use zlmediakit_hls::TsLiveMuxer;

#[allow(clippy::too_many_arguments)]
pub async fn start(
    url: &str,
    vhost: &str,
    app: &str,
    stream_name: &str,
    source_manager: Arc<MediaSourceManager>,
    stop: Arc<Notify>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let source = source_manager
        .get(vhost, app, stream_name)
        .ok_or_else(|| anyhow!("media source not found"))?;
    validate_tracks(&source.info.read().await.tracks)?;

    let mut endpoint = SrtEndpoint::parse(url)?;
    if endpoint.stream_id.is_none() {
        endpoint.stream_id = Some(format!("#!::r={app}/{stream_name},m=publish"));
    }
    let target = endpoint.display_target();
    let socket = Arc::new(
        tokio::task::spawn_blocking(move || SrtSocket::connect(&endpoint, true))
            .await
            .context("SRT caller connect task failed")??,
    );
    info!("SRT push connected: {vhost}/{app}/{stream_name} -> {target}");

    let subscriber_id = format!("srt-push:{target}");
    source.add_subscriber(subscriber_id.clone()).await;
    let mut receiver = source.subscribe();
    let mut muxer = TsLiveMuxer::new();
    for frame in source.get_latest_gop_frames().await {
        send_frame(socket.clone(), &mut muxer, &frame).await?;
    }
    loop {
        let frame = tokio::select! {
            _ = stop.notified() => break,
            frame = receiver.recv() => match frame {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        send_frame(socket.clone(), &mut muxer, &frame).await?;
    }
    source.remove_subscriber(&subscriber_id).await;
    Ok(())
}

fn validate_tracks(tracks: &[TrackInfo]) -> Result<()> {
    let has_video = tracks.iter().any(|track| {
        matches!(track, TrackInfo::Video(video) if matches!(video.codec, CodecId::H264 | CodecId::H265))
    });
    if !has_video {
        bail!("SRT MPEG-TS push requires an H.264 or H.265 video track");
    }
    if tracks
        .iter()
        .any(|track| matches!(track, TrackInfo::Audio(audio) if audio.codec != CodecId::AAC))
    {
        bail!("SRT MPEG-TS push only supports AAC audio");
    }
    Ok(())
}

async fn send_frame(
    socket: Arc<SrtSocket>,
    muxer: &mut TsLiveMuxer,
    frame: &MediaFrame,
) -> Result<()> {
    let output = muxer.push_frame(frame);
    if output.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        for chunk in output.chunks(188 * 7) {
            socket.send_all(chunk)?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("SRT send task failed")??;
    Ok(())
}
