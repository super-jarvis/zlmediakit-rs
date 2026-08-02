use crate::client::{SrtEndpoint, SrtSocket};
use crate::server::TsStreamDemuxer;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::info;
use zlmediakit_core::MediaSourceManager;

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
    let mut endpoint = SrtEndpoint::parse(url)?;
    if endpoint.stream_id.is_none() {
        endpoint.stream_id = Some(format!("#!::r={app}/{stream_name},m=request"));
    }
    let target = endpoint.display_target();
    let socket = Arc::new(
        tokio::task::spawn_blocking(move || SrtSocket::connect(&endpoint, false))
            .await
            .context("SRT caller connect task failed")??,
    );
    info!("SRT caller pull connected: {target} -> {vhost}/{app}/{stream_name}");
    let source = source_manager.get_or_create(vhost, app, stream_name);
    let mut demuxer = TsStreamDemuxer::new();

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let recv_socket = socket.clone();
        let receive = tokio::task::spawn_blocking(move || recv_socket.recv(188 * 64));
        let data = tokio::select! {
            _ = stop.notified() => break,
            result = receive => result.context("SRT receive task failed")??,
        };
        match data {
            Some(data) if !data.is_empty() => demuxer.feed(&data, &source).await,
            Some(_) => continue,
            None => break,
        }
    }
    demuxer.flush(&source).await;
    Ok(())
}
