//! Native WHEP pull client used by stream proxies.
//!
//! The client creates a recv-only PeerConnection, POSTs its complete SDP offer
//! to a remote WHEP resource, applies the returned answer, and republishes the
//! received tracks into the shared `MediaSourceManager`.

use anyhow::{anyhow, bail, Context, Result};
use bytes::BytesMut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::media_source::MediaSourceManager;

use crate::engine::{WebRtcRole, WebRtcTransportSettings};
use crate::whep::build_rtc_config;
use crate::whip::attach_media_receiver;

const MAX_HEADER_SIZE: usize = 64 * 1024;
const MAX_SDP_SIZE: usize = 2 * 1024 * 1024;

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

#[derive(Debug, Clone)]
pub(crate) struct SignallingUrl {
    secure: bool,
    host: String,
    authority: String,
    port: u16,
    path: String,
}

impl SignallingUrl {
    pub(crate) fn parse(url: &str) -> Result<Self> {
        let (secure, rest, default_port) = if let Some(rest) = url.strip_prefix("whep://") {
            (false, rest, 80)
        } else if let Some(rest) = url.strip_prefix("wheps://") {
            (true, rest, 443)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest, 80)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (true, rest, 443)
        } else if let Some(rest) = url.strip_prefix("whip://") {
            (false, rest, 80)
        } else if let Some(rest) = url.strip_prefix("whips://") {
            (true, rest, 443)
        } else {
            bail!("WebRTC HTTP signalling requires WHEP/WHIP or HTTP(S) URL");
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            bail!("WHEP URL is missing a host");
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let end = bracketed
                .find(']')
                .ok_or_else(|| anyhow!("invalid bracketed IPv6 WHEP host"))?;
            let host = bracketed[..end].to_string();
            let suffix = &bracketed[end + 1..];
            let port = suffix
                .strip_prefix(':')
                .map(str::parse)
                .transpose()
                .context("invalid WHEP URL port")?
                .unwrap_or(default_port);
            (host, port)
        } else if authority.matches(':').count() == 1 {
            let (host, port) = authority.rsplit_once(':').expect("count checked");
            (
                host.to_string(),
                port.parse().context("invalid WHEP URL port")?,
            )
        } else {
            (authority.to_string(), default_port)
        };
        Ok(Self {
            secure,
            host,
            authority: authority.to_string(),
            port,
            path: path.to_string(),
        })
    }

    pub(crate) fn as_http_url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}{}", self.authority, self.path)
    }

    pub(crate) fn resolve(&self, reference: &str) -> Result<String> {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Ok(reference.to_string());
        }
        if reference.starts_with("whep://") || reference.starts_with("wheps://") {
            return Ok(reference.to_string());
        }
        let path = if reference.starts_with('/') {
            reference.to_string()
        } else {
            let base = self.path.split('?').next().unwrap_or(&self.path);
            let directory = base.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            format!("{directory}/{reference}")
        };
        let scheme = if self.secure { "https" } else { "http" };
        Ok(format!("{scheme}://{}{}", self.authority, path))
    }
}

pub(crate) struct HttpResponse {
    pub(crate) url: SignallingUrl,
    pub(crate) status: u16,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

async fn connect(url: &SignallingUrl) -> Result<Box<dyn AsyncStream>> {
    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((url.host.as_str(), url.port)),
    )
    .await
    .context("WHEP HTTP connect timed out")?
    .with_context(|| format!("failed to connect {}:{}", url.host, url.port))?;
    if !url.secure {
        return Ok(Box::new(tcp));
    }

    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from(url.host.clone()).context("invalid WHEP TLS name")?;
    let tls = tokio::time::timeout(
        Duration::from_secs(10),
        TlsConnector::from(Arc::new(config)).connect(server_name, tcp),
    )
    .await
    .context("WHEP TLS handshake timed out")?
    .context("WHEP TLS certificate verification or handshake failed")?;
    Ok(Box::new(tls))
}

async fn request_once(method: &str, url: SignallingUrl, body: &[u8]) -> Result<HttpResponse> {
    let mut stream = connect(&url).await?;
    let content_headers = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: zlmediakit-rs/0.1\r\nAccept: application/sdp\r\n{content_headers}Connection: close\r\n\r\n",
        url.path, url.authority
    );
    stream.write_all(request.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }

    let mut buffer = BytesMut::with_capacity(8192);
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() >= MAX_HEADER_SIZE {
            bail!("WHEP response header exceeds {MAX_HEADER_SIZE} bytes");
        }
        let mut chunk = [0u8; 8192];
        let count = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
            .await
            .context("WHEP response header timed out")??;
        if count == 0 {
            bail!("connection closed before WHEP response headers");
        }
        buffer.extend_from_slice(&chunk[..count]);
    };
    let raw_headers = buffer.split_to(header_end + 4);
    let header_text = std::str::from_utf8(&raw_headers[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("invalid WHEP HTTP status line"))?;
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("invalid WHEP Content-Length")?;
    if content_length.is_some_and(|length| length > MAX_SDP_SIZE) {
        bail!("WHEP response body exceeds {MAX_SDP_SIZE} bytes");
    }
    if let Some(length) = content_length {
        while buffer.len() < length {
            let mut chunk = [0u8; 8192];
            let count = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
                .await
                .context("WHEP response body timed out")??;
            if count == 0 {
                bail!("truncated WHEP response body");
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.len() > MAX_SDP_SIZE {
                bail!("WHEP response body exceeds {MAX_SDP_SIZE} bytes");
            }
        }
        buffer.truncate(length);
    } else {
        loop {
            let mut chunk = [0u8; 8192];
            let count = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk))
                .await
                .context("WHEP response body timed out")??;
            if count == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.len() > MAX_SDP_SIZE {
                bail!("WHEP response body exceeds {MAX_SDP_SIZE} bytes");
            }
        }
    }
    Ok(HttpResponse {
        url,
        status,
        headers,
        body: buffer.to_vec(),
    })
}

pub(crate) async fn request(method: &str, url: &str, body: &[u8]) -> Result<HttpResponse> {
    let mut current = url.to_string();
    for _ in 0..=5 {
        let parsed = SignallingUrl::parse(&current)?;
        let response = request_once(method, parsed, body).await?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers
                .get("location")
                .ok_or_else(|| anyhow!("WHEP redirect is missing Location"))?;
            current = response.url.resolve(location)?;
            continue;
        }
        return Ok(response);
    }
    bail!("too many WHEP HTTP redirects")
}

/// Pull a remote WHEP resource and republish its RTP tracks locally.
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
    info!("WHEP pull: {target} -> {vhost}/{app}/{stream_name}");

    let api = crate::engine::build_api_with_transport(
        WebRtcRole::Receiver,
        &WebRtcTransportSettings::default(),
    )?;
    let pc = Arc::new(
        api.build()
            .new_peer_connection(build_rtc_config(&ice_servers))
            .await?,
    );
    let source = source_manager.get_or_create(vhost, app, stream_name);
    attach_media_receiver(&pc, source);

    let gathering_done = Arc::new(Notify::new());
    let gathering_tx = gathering_done.clone();
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let notify = gathering_tx.clone();
        Box::pin(async move {
            if candidate.is_none() {
                notify.notify_one();
            }
        })
    }));
    for kind in [RTPCodecType::Video, RTPCodecType::Audio] {
        pc.add_transceiver_from_kind(
            kind,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await?;
    }
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    gathering_done.notified().await;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("WHEP pull has no local description after ICE gathering"))?
        .sdp;

    let response = request("POST", &target, offer_sdp.as_bytes()).await?;
    if response.status != 201 {
        bail!(
            "WHEP upstream returned status {}: {}",
            response.status,
            String::from_utf8_lossy(&response.body)
        );
    }
    let location = response
        .headers
        .get("location")
        .ok_or_else(|| anyhow!("WHEP response is missing Location"))?;
    let resource_url = response.url.resolve(location)?;
    let answer_sdp = String::from_utf8(response.body).context("WHEP answer is not UTF-8 SDP")?;
    pc.set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;
    info!("WHEP pull negotiated: {resource_url}");

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
            debug!(status = response.status, "WHEP upstream resource deleted");
        }
        Ok(Ok(response)) => warn!(status = response.status, "WHEP DELETE was rejected"),
        Ok(Err(error)) => warn!("WHEP DELETE failed: {error}"),
        Err(_) => warn!("WHEP DELETE timed out"),
    }
    pc.close().await?;
    info!("WHEP pull finished for {target}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_ipv6() {
        let url = SignallingUrl::parse("wheps://[2001:db8::1]:8443/webrtc/play/v/a/s").unwrap();
        assert!(url.secure);
        assert_eq!(url.host, "2001:db8::1");
        assert_eq!(url.port, 8443);
        assert_eq!(
            url.as_http_url(),
            "https://[2001:db8::1]:8443/webrtc/play/v/a/s"
        );
        assert_eq!(
            url.resolve("/webrtc/play/resource").unwrap(),
            "https://[2001:db8::1]:8443/webrtc/play/resource"
        );
    }
}
