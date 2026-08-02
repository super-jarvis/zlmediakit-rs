//! Minimal HTTP transport for WHEP (playback) and WHIP (publishing).
//!
//! Both are tiny HTTP protocols: the client POSTs an SDP offer and gets an SDP
//! answer back (plus a `Location` it later DELETEs to stop). We implement just
//! enough HTTP/1.1 here so the WebRTC crate stays decoupled from the `http`
//! protocol crate (which, per project convention, must not depend on WebRTC).

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use dashmap::DashMap;
use ice::udp_mux::{UDPMux, UDPMuxDefault, UDPMuxParams};
use ice::udp_network::UDPNetwork;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{error, info};
use uuid::Uuid;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use zlmediakit_core::config::IceServer;
use zlmediakit_core::media_source::MediaSourceManager;

use crate::engine::WebRtcTransportSettings;
use crate::whep::{whep_play_with_transport, WhepSession};
use crate::whip::{whip_publish_with_transport, WhipSession};

#[derive(Debug, Clone)]
pub struct WebRtcTransportConfig {
    pub udp_bind_addr: SocketAddr,
    pub ice_lite: bool,
    pub external_ips: Vec<String>,
}

pub struct WebRtcServer {
    listener: TcpListener,
    source_manager: Arc<MediaSourceManager>,
    ice_servers: Vec<IceServer>,
    transport: Arc<WebRtcTransportSettings>,
    play_sessions: Arc<DashMap<String, WhepSession>>,
    publish_sessions: Arc<DashMap<String, WhipSession>>,
}

impl WebRtcServer {
    /// Actual TCP signalling address, including the assigned port when bound
    /// with port zero. Primarily useful for embedding and network E2E tests.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub async fn new(
        addr: &str,
        source_manager: Arc<MediaSourceManager>,
        ice_servers: Vec<IceServer>,
    ) -> Result<Self> {
        crate::init_crypto();
        let listener = TcpListener::bind(addr).await?;
        info!("WebRTC (WHEP/WHIP) server listening on {}", addr);
        Ok(Self {
            listener,
            source_manager,
            ice_servers,
            transport: Arc::new(WebRtcTransportSettings::default()),
            play_sessions: Arc::new(DashMap::new()),
            publish_sessions: Arc::new(DashMap::new()),
        })
    }

    /// Start signalling plus a process-wide UDP mux used by every ICE session.
    /// This keeps firewall/NAT deployment to one predictable UDP port.
    pub async fn new_with_transport(
        addr: &str,
        source_manager: Arc<MediaSourceManager>,
        ice_servers: Vec<IceServer>,
        config: WebRtcTransportConfig,
    ) -> Result<Self> {
        crate::init_crypto();
        let listener = TcpListener::bind(addr).await?;
        let udp_socket = UdpSocket::bind(config.udp_bind_addr).await?;
        let local_udp = udp_socket.local_addr()?;
        let bind_ip =
            (!config.udp_bind_addr.ip().is_unspecified()).then_some(config.udp_bind_addr.ip());
        let udp_mux: Arc<dyn UDPMux + Send + Sync> =
            UDPMuxDefault::new(UDPMuxParams::new(udp_socket));
        info!("WebRTC (WHEP/WHIP) server listening on {addr}");
        info!("WebRTC ICE UDP mux listening on {local_udp}");
        Ok(Self {
            listener,
            source_manager,
            ice_servers,
            transport: Arc::new(WebRtcTransportSettings {
                udp_network: Some(UDPNetwork::Muxed(udp_mux)),
                bind_ip,
                ice_lite: config.ice_lite,
                external_ips: config.external_ips,
            }),
            play_sessions: Arc::new(DashMap::new()),
            publish_sessions: Arc::new(DashMap::new()),
        })
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, _peer) = self.listener.accept().await?;
            let sm = self.source_manager.clone();
            let ice = self.ice_servers.clone();
            let transport = self.transport.clone();
            let play = self.play_sessions.clone();
            let pub_ = self.publish_sessions.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, sm, ice, transport, play, pub_).await {
                    error!("webrtc: conn error: {}", e);
                }
            });
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    sm: Arc<MediaSourceManager>,
    ice_servers: Vec<IceServer>,
    transport: Arc<WebRtcTransportSettings>,
    play_sessions: Arc<DashMap<String, WhepSession>>,
    publish_sessions: Arc<DashMap<String, WhipSession>>,
) -> Result<()> {
    let mut buf = BytesMut::new();
    let mut tmp = [0u8; 8192];

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return write_response(
                &mut stream,
                400,
                "Bad Request",
                &[],
                b"connection closed before request completed",
            )
            .await;
        }
        buf.extend_from_slice(&tmp[..n]);

        let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
        if let Some(h) = header_end {
            let body_len = content_length(&buf);
            if buf.len() >= h + body_len {
                break;
            }
        }
        if buf.len() > 1 << 20 {
            return write_response(
                &mut stream,
                413,
                "Request Too Large",
                &[],
                b"request exceeds maximum size",
            )
            .await;
        }
    }

    let (method, path, query, body) = parse_request(&buf)?;
    match method.as_str() {
        "POST" => {
            handle_post(
                &mut stream,
                &path,
                &query,
                &body,
                &WebRtcState {
                    sm: &sm,
                    ice_servers: &ice_servers,
                    transport: &transport,
                    play_sessions: &play_sessions,
                    publish_sessions: &publish_sessions,
                },
            )
            .await
        }
        "OPTIONS" => {
            write_response(
                &mut stream,
                204,
                "No Content",
                &[
                    (
                        "Access-Control-Allow-Methods",
                        "POST, PATCH, DELETE, OPTIONS",
                    ),
                    (
                        "Access-Control-Allow-Headers",
                        "Content-Type, Authorization",
                    ),
                    ("Access-Control-Max-Age", "86400"),
                ],
                &[],
            )
            .await
        }
        "PATCH" => handle_patch(&mut stream, &path, &body, &play_sessions, &publish_sessions).await,
        "DELETE" => handle_delete(&mut stream, &path, &sm, &play_sessions, &publish_sessions).await,
        _ => write_response(&mut stream, 405, "Method Not Allowed", &[], &[]).await,
    }
}

fn resolve_stream(segs: &[&str], query: &HashMap<String, String>) -> (String, String, String) {
    if segs.len() >= 3 {
        (
            segs[0].to_string(),
            segs[1].to_string(),
            segs[2].to_string(),
        )
    } else {
        (
            query
                .get("vhost")
                .cloned()
                .unwrap_or_else(|| "__defaultVhost__".to_string()),
            query
                .get("app")
                .cloned()
                .unwrap_or_else(|| "live".to_string()),
            query.get("stream").cloned().unwrap_or_default(),
        )
    }
}

/// Shared server state threaded through every request handler, bundled so the
/// handler signatures stay readable.
struct WebRtcState<'a> {
    sm: &'a Arc<MediaSourceManager>,
    ice_servers: &'a [IceServer],
    transport: &'a WebRtcTransportSettings,
    play_sessions: &'a Arc<DashMap<String, WhepSession>>,
    publish_sessions: &'a Arc<DashMap<String, WhipSession>>,
}

fn watch_whep_session(
    pc: Arc<RTCPeerConnection>,
    resource: String,
    sessions: Arc<DashMap<String, WhepSession>>,
) {
    let observed_pc = pc.clone();
    pc.on_peer_connection_state_change(Box::new(move |state| {
        let observed_pc = observed_pc.clone();
        let resource = resource.clone();
        let sessions = sessions.clone();
        Box::pin(async move {
            let grace = state == RTCPeerConnectionState::Disconnected;
            if !grace
                && state != RTCPeerConnectionState::Failed
                && state != RTCPeerConnectionState::Closed
            {
                return;
            }
            tokio::spawn(async move {
                if grace {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                let current = observed_pc.connection_state();
                let should_close = matches!(
                    current,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) || (grace && current == RTCPeerConnectionState::Disconnected);
                if should_close {
                    if let Some((_, session)) = sessions.remove(&resource) {
                        session.close().await;
                        info!("webrtc: WHEP session {resource} cleaned up after {current}");
                    }
                }
            });
        })
    }));
}

fn watch_whip_session(
    pc: Arc<RTCPeerConnection>,
    resource: String,
    sessions: Arc<DashMap<String, WhipSession>>,
    source_manager: Arc<MediaSourceManager>,
) {
    let observed_pc = pc.clone();
    pc.on_peer_connection_state_change(Box::new(move |state| {
        let observed_pc = observed_pc.clone();
        let resource = resource.clone();
        let sessions = sessions.clone();
        let source_manager = source_manager.clone();
        Box::pin(async move {
            let grace = state == RTCPeerConnectionState::Disconnected;
            if !grace
                && state != RTCPeerConnectionState::Failed
                && state != RTCPeerConnectionState::Closed
            {
                return;
            }
            tokio::spawn(async move {
                if grace {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                let current = observed_pc.connection_state();
                let should_close = matches!(
                    current,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) || (grace && current == RTCPeerConnectionState::Disconnected);
                if should_close {
                    if let Some((_, session)) = sessions.remove(&resource) {
                        let (vhost, app, stream) = session.source_identity();
                        session.close().await;
                        source_manager.close_stream(&vhost, &app, &stream);
                        info!("webrtc: WHIP session {resource} cleaned up after {current}");
                    }
                }
            });
        })
    }));
}

async fn handle_post(
    stream: &mut TcpStream,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    state: &WebRtcState<'_>,
) -> Result<()> {
    if path.starts_with("/webrtc/play/") {
        handle_play(stream, path, query, body, state).await
    } else if path.starts_with("/webrtc/publish/") {
        handle_publish(stream, path, query, body, state).await
    } else {
        write_response(stream, 404, "Not Found", &[], b"unknown endpoint").await
    }
}

fn play_segments(path: &str) -> Vec<String> {
    path.trim_start_matches("/webrtc/play/")
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn publish_segments(path: &str) -> Vec<String> {
    path.trim_start_matches("/webrtc/publish/")
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

async fn handle_play(
    stream: &mut TcpStream,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    state: &WebRtcState<'_>,
) -> Result<()> {
    let segs: Vec<String> = play_segments(path);
    let segs_ref: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    let (vhost, app, stream_id) = resolve_stream(&segs_ref, query);

    if stream_id.is_empty() {
        return write_response(
            stream,
            400,
            "Bad Request",
            &[],
            b"missing stream identifier",
        )
        .await;
    }

    let source = match state.sm.get_or_pull(&vhost, &app, &stream_id).await {
        Some(s) => s,
        None => {
            return write_response(stream, 404, "Not Found", &[], b"stream not found").await;
        }
    };
    if let Some(requested_rid) = query.get("rid") {
        let available = source
            .track_descriptors
            .read()
            .await
            .values()
            .any(|descriptor| descriptor.rid.as_deref() == Some(requested_rid.as_str()));
        if !available {
            return write_response(stream, 404, "Not Found", &[], b"simulcast RID not found").await;
        }
    }

    let offer = String::from_utf8_lossy(body).to_string();
    let resource = Uuid::new_v4().to_string();
    match whep_play_with_transport(
        &offer,
        source,
        resource.clone(),
        state.ice_servers.to_vec(),
        state.transport,
        query.get("rid").map(String::as_str),
    )
    .await
    {
        Ok((answer, session)) => {
            let pc = session.pc.clone();
            state.play_sessions.insert(resource.clone(), session);
            watch_whep_session(pc, resource.clone(), state.play_sessions.clone());
            let location = format!("/webrtc/play/{}", resource);
            write_response(
                stream,
                201,
                "Created",
                &[
                    ("Content-Type", "application/sdp"),
                    ("Location", &location),
                    ("ETag", &resource),
                    ("Accept-Patch", "application/trickle-ice-sdpfrag"),
                    ("Access-Control-Allow-Origin", "*"),
                ],
                answer.as_bytes(),
            )
            .await
        }
        Err(e) => {
            error!(
                "webrtc: whep_play failed for {}/{}/{}: {}",
                vhost, app, stream_id, e
            );
            write_response(
                stream,
                500,
                "Internal Server Error",
                &[],
                b"negotiation failed",
            )
            .await
        }
    }
}

async fn handle_publish(
    stream: &mut TcpStream,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    state: &WebRtcState<'_>,
) -> Result<()> {
    let segs: Vec<String> = publish_segments(path);
    let segs_ref: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    let (vhost, app, stream_id) = resolve_stream(&segs_ref, query);

    if stream_id.is_empty() {
        return write_response(
            stream,
            400,
            "Bad Request",
            &[],
            b"missing stream identifier",
        )
        .await;
    }

    let source = state.sm.get_or_create(&vhost, &app, &stream_id);
    let offer = String::from_utf8_lossy(body).to_string();
    let resource = Uuid::new_v4().to_string();
    match whip_publish_with_transport(
        &offer,
        source,
        resource.clone(),
        state.ice_servers.to_vec(),
        state.transport,
    )
    .await
    {
        Ok((answer, session)) => {
            let pc = session.pc.clone();
            state.publish_sessions.insert(resource.clone(), session);
            watch_whip_session(
                pc,
                resource.clone(),
                state.publish_sessions.clone(),
                state.sm.clone(),
            );
            let location = format!("/webrtc/publish/{}", resource);
            write_response(
                stream,
                201,
                "Created",
                &[
                    ("Content-Type", "application/sdp"),
                    ("Location", &location),
                    ("ETag", &resource),
                    ("Accept-Patch", "application/trickle-ice-sdpfrag"),
                    ("Access-Control-Allow-Origin", "*"),
                ],
                answer.as_bytes(),
            )
            .await
        }
        Err(e) => {
            error!(
                "webrtc: whip_publish failed for {}/{}/{}: {}",
                vhost, app, stream_id, e
            );
            write_response(
                stream,
                500,
                "Internal Server Error",
                &[],
                b"negotiation failed",
            )
            .await
        }
    }
}

async fn handle_patch(
    stream: &mut TcpStream,
    path: &str,
    body: &[u8],
    play_sessions: &Arc<DashMap<String, WhepSession>>,
    publish_sessions: &Arc<DashMap<String, WhipSession>>,
) -> Result<()> {
    let session = if let Some(resource) = path.strip_prefix("/webrtc/play/") {
        play_sessions
            .get(resource)
            .map(|session| (session.pc.clone(), session.negotiation_lock()))
    } else if let Some(resource) = path.strip_prefix("/webrtc/publish/") {
        publish_sessions
            .get(resource)
            .map(|session| (session.pc.clone(), session.negotiation_lock()))
    } else {
        None
    };
    let Some((pc, negotiation)) = session else {
        return write_response(stream, 404, "Not Found", &[], b"session not found").await;
    };
    let _negotiation_guard = negotiation.lock().await;

    let fragment = String::from_utf8_lossy(body);
    let restart_credentials = match parse_ice_credentials(&fragment) {
        Ok(credentials) => credentials,
        Err(e) => {
            error!("webrtc: invalid ICE restart fragment: {e}");
            return write_response(stream, 400, "Bad Request", &[], b"invalid ICE credentials")
                .await;
        }
    };
    let candidates = match parse_sdpfrag_candidates(&fragment) {
        Ok(candidates) => candidates,
        Err(e) => {
            error!("webrtc: invalid trickle ICE fragment: {e}");
            return write_response(
                stream,
                400,
                "Bad Request",
                &[],
                b"invalid trickle ICE fragment",
            )
            .await;
        }
    };

    if let Some((username_fragment, password)) = restart_credentials {
        let answer_fragment =
            match restart_ice(&pc, &username_fragment, &password, candidates).await {
                Ok(fragment) => fragment,
                Err(e) => {
                    error!("webrtc: ICE restart failed: {e}");
                    return write_response(
                        stream,
                        409,
                        "Conflict",
                        &[],
                        b"ICE restart negotiation failed",
                    )
                    .await;
                }
            };
        return write_response(
            stream,
            200,
            "OK",
            &[
                ("Content-Type", "application/trickle-ice-sdpfrag"),
                ("Accept-Patch", "application/trickle-ice-sdpfrag"),
            ],
            answer_fragment.as_bytes(),
        )
        .await;
    }

    for candidate in candidates {
        if let Err(e) = pc.add_ice_candidate(candidate).await {
            error!("webrtc: failed to add trickle ICE candidate: {e}");
            return write_response(stream, 400, "Bad Request", &[], b"invalid ICE candidate").await;
        }
    }
    write_response(
        stream,
        204,
        "No Content",
        &[("Accept-Patch", "application/trickle-ice-sdpfrag")],
        &[],
    )
    .await
}

fn parse_ice_credentials(fragment: &str) -> Result<Option<(String, String)>> {
    let username_fragment = fragment
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .map(str::to_owned);
    let password = fragment
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-pwd:"))
        .map(str::to_owned);
    match (username_fragment, password) {
        (Some(username_fragment), Some(password)) => Ok(Some((username_fragment, password))),
        (None, None) | (Some(_), None) => Ok(None),
        (None, Some(_)) => Err(anyhow!("ICE password requires a username fragment")),
    }
}

fn replace_remote_ice_credentials(
    sdp: &str,
    username_fragment: &str,
    password: &str,
) -> Result<String> {
    let mut replaced_username = false;
    let mut replaced_password = false;
    let mut output = String::with_capacity(sdp.len());
    for line in sdp.lines().map(str::trim_end) {
        if line.starts_with("a=ice-ufrag:") {
            output.push_str("a=ice-ufrag:");
            output.push_str(username_fragment);
            replaced_username = true;
        } else if line.starts_with("a=ice-pwd:") {
            output.push_str("a=ice-pwd:");
            output.push_str(password);
            replaced_password = true;
        } else if line.starts_with("a=candidate:") || line == "a=end-of-candidates" {
            continue;
        } else {
            output.push_str(line);
        }
        output.push_str("\r\n");
    }
    if !replaced_username || !replaced_password {
        return Err(anyhow!(
            "current remote SDP has no replaceable ICE credentials"
        ));
    }
    Ok(output)
}

fn build_sdp_fragment(sdp: &str) -> Result<String> {
    let username_fragment = sdp
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .ok_or_else(|| anyhow!("local SDP has no ICE username fragment"))?;
    let password = sdp
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-pwd:"))
        .ok_or_else(|| anyhow!("local SDP has no ICE password"))?;
    let mut output = format!("a=ice-ufrag:{username_fragment}\r\na=ice-pwd:{password}\r\n");
    let mut in_media = false;
    for line in sdp.lines().map(str::trim_end) {
        if line.starts_with("m=") {
            in_media = true;
            output.push_str(line);
            output.push_str("\r\n");
        } else if in_media
            && (line.starts_with("a=mid:")
                || line.starts_with("a=candidate:")
                || line == "a=end-of-candidates")
        {
            output.push_str(line);
            output.push_str("\r\n");
        }
    }
    Ok(output)
}

async fn restart_ice(
    pc: &Arc<RTCPeerConnection>,
    username_fragment: &str,
    password: &str,
    candidates: Vec<RTCIceCandidateInit>,
) -> Result<String> {
    let remote = pc
        .remote_description()
        .await
        .ok_or_else(|| anyhow!("peer has no remote description"))?;
    let restarted_offer = replace_remote_ice_credentials(&remote.sdp, username_fragment, password)?;
    pc.set_remote_description(RTCSessionDescription::offer(restarted_offer)?)
        .await?;
    for candidate in candidates {
        pc.add_ice_candidate(candidate).await?;
    }

    pc.restart_ice().await?;
    let mut gathering_complete = pc.gathering_complete_promise().await;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        gathering_complete.recv(),
    )
    .await
    .map_err(|_| anyhow!("ICE restart candidate gathering timed out"))?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("peer has no local description after ICE restart"))?;
    build_sdp_fragment(&local.sdp)
}

fn parse_sdpfrag_candidates(fragment: &str) -> Result<Vec<RTCIceCandidateInit>> {
    let username_fragment = fragment
        .lines()
        .find_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .map(str::to_owned);
    let mut media_index: Option<u16> = None;
    let mut media_mid: Option<String> = None;
    let mut candidates = Vec::new();
    let mut end_of_candidates = false;
    for line in fragment.lines().map(str::trim) {
        if line.starts_with("m=") {
            media_index = Some(media_index.map_or(0, |index| index.saturating_add(1)));
            media_mid = None;
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            media_mid = Some(mid.to_owned());
        } else if let Some(candidate) = line.strip_prefix("a=candidate:") {
            candidates.push(RTCIceCandidateInit {
                candidate: format!("candidate:{candidate}"),
                sdp_mid: media_mid.clone(),
                sdp_mline_index: media_index,
                username_fragment: username_fragment.clone(),
            });
        } else if line == "a=end-of-candidates" {
            end_of_candidates = true;
        }
    }
    let has_restart_credentials = parse_ice_credentials(fragment)?.is_some();
    if candidates.is_empty() && !end_of_candidates && !has_restart_credentials {
        return Err(anyhow!("SDP fragment has no ICE candidate"));
    }
    Ok(candidates)
}

async fn handle_delete(
    stream: &mut TcpStream,
    path: &str,
    source_manager: &Arc<MediaSourceManager>,
    play_sessions: &Arc<DashMap<String, WhepSession>>,
    publish_sessions: &Arc<DashMap<String, WhipSession>>,
) -> Result<()> {
    let resource = path
        .trim_start_matches("/webrtc/play/")
        .trim_start_matches("/webrtc/publish/")
        .trim_end_matches('/')
        .to_string();
    if let Some((_, session)) = play_sessions.remove(&resource) {
        session.close().await;
        return write_response(stream, 200, "OK", &[], b"stopped").await;
    }
    if let Some((_, session)) = publish_sessions.remove(&resource) {
        let (vhost, app, source_stream) = session.source_identity();
        session.close().await;
        source_manager.close_stream(&vhost, &app, &source_stream);
        return write_response(stream, 200, "OK", &[], b"stopped").await;
    }
    write_response(stream, 404, "Not Found", &[], b"unknown resource").await
}

fn content_length(buf: &BytesMut) -> usize {
    let head = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => &buf[..p],
        None => return 0,
    };
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if let Some(val) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            if let Ok(n) = val.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

#[allow(clippy::type_complexity)]
fn parse_request(buf: &BytesMut) -> Result<(String, String, HashMap<String, String>, Vec<u8>)> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, HashMap::new()),
    };

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let body = buf[header_end..].to_vec();

    Ok((method, path, query, body))
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.to_string(), v.replace('+', " "));
        }
    }
    map
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let mut resp = format!("HTTP/1.1 {} {}\r\n", status, reason);
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    // Always include CORS headers so browser clients can read error responses.
    resp.push_str("Access-Control-Allow-Origin: *\r\n");
    for (k, v) in headers {
        resp.push_str(&format!("{}: {}\r\n", k, v));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trickle_ice_sdp_fragment_per_media_section() {
        let fragment = "a=ice-ufrag:remote\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
a=mid:video0\r\n\
a=candidate:1 1 udp 2122260223 192.0.2.10 5000 typ host\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:audio0\r\n\
a=candidate:2 1 udp 2122260223 192.0.2.11 5002 typ host\r\n";
        let candidates = parse_sdpfrag_candidates(fragment).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].sdp_mid.as_deref(), Some("video0"));
        assert_eq!(candidates[0].sdp_mline_index, Some(0));
        assert_eq!(candidates[1].sdp_mid.as_deref(), Some("audio0"));
        assert_eq!(candidates[1].sdp_mline_index, Some(1));
        assert_eq!(candidates[1].username_fragment.as_deref(), Some("remote"));
    }

    #[test]
    fn accepts_end_of_candidates_and_rejects_empty_fragment() {
        assert!(parse_sdpfrag_candidates("a=end-of-candidates\r\n")
            .unwrap()
            .is_empty());
        assert!(parse_sdpfrag_candidates("a=ice-ufrag:remote\r\n").is_err());
    }

    #[test]
    fn replaces_credentials_and_builds_restart_fragment() {
        let sdp = "v=0\r\n\
a=ice-ufrag:old-user\r\n\
a=ice-pwd:old-password\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
a=mid:0\r\n\
a=candidate:1 1 udp 1 192.0.2.1 5000 typ host\r\n\
a=end-of-candidates\r\n";
        let replaced = replace_remote_ice_credentials(sdp, "new-user", "new-password").unwrap();
        assert!(replaced.contains("a=ice-ufrag:new-user\r\n"));
        assert!(replaced.contains("a=ice-pwd:new-password\r\n"));
        assert!(!replaced.contains("a=candidate:"));
        assert!(!replaced.contains("a=end-of-candidates"));

        let fragment = build_sdp_fragment(sdp).unwrap();
        assert!(fragment.starts_with("a=ice-ufrag:old-user\r\na=ice-pwd:old-password\r\n"));
        assert!(fragment.contains("m=video 9 UDP/TLS/RTP/SAVPF 102\r\n"));
        assert!(fragment.contains("a=mid:0\r\n"));
        assert!(fragment.contains("a=candidate:1 1 udp 1 192.0.2.1 5000 typ host\r\n"));
    }

    #[test]
    fn recognizes_complete_restart_credentials() {
        assert_eq!(
            parse_ice_credentials("a=ice-ufrag:new\r\na=ice-pwd:secret\r\n").unwrap(),
            Some(("new".to_string(), "secret".to_string()))
        );
        assert!(
            parse_sdpfrag_candidates("a=ice-ufrag:new\r\na=ice-pwd:secret\r\n")
                .unwrap()
                .is_empty()
        );
        assert!(parse_ice_credentials("a=ice-pwd:secret\r\n").is_err());
    }
}
