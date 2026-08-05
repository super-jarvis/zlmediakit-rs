use super::parser::{RtspParser, RtspRequest, RtspResponse};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use rand::RngExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::{Event, EventBus};
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{
    AudioInfo, CodecId, FrameType, MediaFrame, PayloadFormat, TrackInfo, VideoInfo,
};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};
use zlmediakit_core::transport::TransportStream;
use zlmediakit_core::{aac_raw_payload, flv_payload};
use zlmediakit_mp4::{Mp4VodLibrary, VodAsset, VodPacer};

pub struct RtspSession {
    stream: Option<TransportStream>,
    peer_addr: String,
    client_ip: String,
    source_manager: Arc<MediaSourceManager>,
    event_bus: Arc<EventBus>,
    auth: Arc<StreamAuth>,
    hook: Option<Arc<HookClient>>,
    realm: Option<String>,
    nonce: String,
    source: Option<Arc<MediaSource>>,
    vod_library: Option<Arc<Mp4VodLibrary>>,
    vod_asset: Option<VodAsset>,
    cseq: u32,
    session_id: String,
    app: String,
    stream_name: String,
    stream_sign: String,
    is_publisher: bool,
    tcp_interleaved: bool,
    setup_count: u32,
    udp_video: Option<Arc<UdpSocket>>,
    udp_audio: Option<Arc<UdpSocket>>,
    play_stop: Option<Arc<Notify>>,
    buffer: BytesMut,

    // RTSP push (publish) support
    video_pt: u8,
    audio_pt: u8,
    video_chan: u8,
    audio_chan: u8,
    audio_sample_rate: u32,
    video_codec: CodecId,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    vps: Option<Vec<u8>>,
    audio_config: Option<Vec<u8>>,
    depak: Option<RtpDepacketizer>,
}

impl RtspSession {
    pub fn new(
        stream: TransportStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
        event_bus: Arc<EventBus>,
        auth: Arc<StreamAuth>,
        hook: Option<Arc<HookClient>>,
    ) -> Self {
        let session_id = format!("{:08x}", rand::rng().random::<u32>());
        let nonce = format!(
            "{:08x}{:08x}",
            rand::rng().random::<u32>(),
            rand::rng().random::<u32>()
        );
        let client_ip = peer_addr
            .split(':')
            .next()
            .unwrap_or("127.0.0.1")
            .to_string();
        Self {
            stream: Some(stream),
            peer_addr,
            client_ip,
            source_manager,
            event_bus,
            auth,
            hook,
            realm: None,
            nonce,
            source: None,
            vod_library: None,
            vod_asset: None,
            cseq: 0,
            session_id,
            app: String::new(),
            stream_name: String::new(),
            stream_sign: String::new(),
            is_publisher: false,
            tcp_interleaved: false,
            setup_count: 0,
            udp_video: None,
            udp_audio: None,
            play_stop: None,
            buffer: BytesMut::with_capacity(4096),

            video_pt: 96,
            audio_pt: 97,
            video_chan: 0,
            audio_chan: 2,
            audio_sample_rate: 44100,
            video_codec: CodecId::H264,
            sps: None,
            pps: None,
            vps: None,
            audio_config: None,
            depak: None,
        }
    }

    pub fn with_vod_library(mut self, vod: Option<Arc<Mp4VodLibrary>>) -> Self {
        self.vod_library = vod;
        self
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        info!("RTSP session started from {}", self.peer_addr);

        let mut read_buf = [0u8; 4096];
        loop {
            if self.stream.is_none() {
                break;
            }
            let stream = match self.stream.as_mut() {
                Some(s) => s,
                None => break,
            };
            match stream.read(&mut read_buf).await {
                Ok(0) => {
                    debug!("RTSP connection closed: {}", self.peer_addr);
                    break;
                }
                Ok(n) => {
                    self.buffer.extend_from_slice(&read_buf[..n]);
                    self.process_buffer().await?;
                }
                Err(e) => {
                    error!("RTSP read error: {}", e);
                    break;
                }
            }
        }

        self.cleanup().await;
        Ok(())
    }

    async fn process_buffer(&mut self) -> anyhow::Result<()> {
        loop {
            // During publishing over TCP-interleaved, RTP arrives framed with '$'
            // on the same connection. Demux it before trying to parse RTSP requests.
            if self.is_publisher && self.tcp_interleaved && self.buffer.first() == Some(&b'$') {
                if self.buffer.len() < 4 {
                    break;
                }
                let channel = self.buffer[1];
                let len = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
                if self.buffer.len() < 4 + len {
                    break;
                }
                let payload = self.buffer[4..4 + len].to_vec();
                self.buffer.advance(4 + len);
                if let Some(ref mut depak) = self.depak {
                    depak.handle_rtp(channel, &payload).await;
                }
                continue;
            }
            if let Some((request, consumed)) = RtspParser::parse_request(&self.buffer) {
                self.buffer.advance(consumed);
                self.handle_request(request).await?;
            } else {
                break;
            }
        }
        Ok(())
    }

    async fn handle_request(&mut self, request: RtspRequest) -> anyhow::Result<()> {
        self.cseq = request
            .headers
            .get("CSeq")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        info!(
            "RTSP {} {} CSeq: {}",
            request.method, request.uri, self.cseq
        );

        match request.method.as_str() {
            "OPTIONS" => self.handle_options().await,
            "DESCRIBE" => self.handle_describe(&request).await,
            "SETUP" => self.handle_setup(&request).await,
            "PLAY" => self.handle_play(&request).await,
            "ANNOUNCE" => self.handle_announce(&request).await,
            "RECORD" => self.handle_record().await,
            "TEARDOWN" => self.handle_teardown().await,
            _ => {
                warn!("Unsupported RTSP method: {}", request.method);
                let response = RtspResponse::new(405, "Method Not Allowed");
                self.send_response(&response).await
            }
        }
    }

    async fn handle_options(&mut self) -> anyhow::Result<()> {
        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, ANNOUNCE, RECORD, TEARDOWN",
            );
        self.send_response(&response).await
    }

    async fn handle_describe(&mut self, request: &RtspRequest) -> anyhow::Result<()> {
        self.parse_uri(&request.uri);
        info!(
            "RTSP DESCRIBE: app={}, stream={}",
            self.app, self.stream_name
        );

        if !self.is_authenticated(request, "play").await {
            warn!(
                "RTSP play rejected (auth): {}/{}",
                self.app, self.stream_name
            );
            let resp = self.auth_reject(request).await;
            return self.send_response(&resp).await;
        }

        let source = self
            .source_manager
            .get_or_pull("__defaultVhost__", &self.app, &self.stream_name)
            .await;

        info!("RTSP DESCRIBE source found: {}", source.is_some());

        match source {
            Some(source) => {
                self.source = Some(source.clone());

                let video_codec = {
                    let info = source.info.read().await;
                    info.tracks
                        .iter()
                        .find_map(|t| match t {
                            TrackInfo::Video(v) => Some(v.codec),
                            _ => None,
                        })
                        .unwrap_or(CodecId::H264)
                };
                let hevc_sprop = if video_codec == CodecId::H265 {
                    extract_hevc_sprop_from_source(&source).await
                } else {
                    None
                };
                let audio_track = {
                    let info = source.info.read().await;
                    info.tracks.iter().find_map(|t| match t {
                        TrackInfo::Audio(a) => Some(a.clone()),
                        _ => None,
                    })
                };
                let sdp = self.generate_sdp(video_codec, hevc_sprop, audio_track.as_ref());
                let response = RtspResponse::new(200, "OK")
                    .with_header("CSeq", &self.cseq.to_string())
                    .with_header("Content-Type", "application/sdp")
                    .with_body(sdp.into_bytes());
                self.send_response(&response).await
            }
            None => {
                let asset = self.open_vod().await;
                match asset {
                    Some(asset) => {
                        let info = asset.media_info();
                        let video_codec = info
                            .tracks
                            .iter()
                            .find_map(|track| match track {
                                TrackInfo::Video(video) => Some(video.codec),
                                _ => None,
                            })
                            .unwrap_or(CodecId::H264);
                        let audio = info.tracks.iter().find_map(|track| match track {
                            TrackInfo::Audio(audio) => Some(audio.clone()),
                            _ => None,
                        });
                        let hevc_sprop = if video_codec == CodecId::H265 {
                            extract_hevc_sprop_from_asset(&asset)
                        } else {
                            None
                        };
                        let sdp = self.generate_sdp(video_codec, hevc_sprop, audio.as_ref());
                        self.vod_asset = Some(asset);
                        let response = RtspResponse::new(200, "OK")
                            .with_header("CSeq", &self.cseq.to_string())
                            .with_header("Content-Type", "application/sdp")
                            .with_body(sdp.into_bytes());
                        self.send_response(&response).await
                    }
                    None => {
                        let response = RtspResponse::new(404, "Not Found")
                            .with_header("CSeq", &self.cseq.to_string());
                        self.send_response(&response).await
                    }
                }
            }
        }
    }

    async fn handle_setup(&mut self, request: &RtspRequest) -> anyhow::Result<()> {
        if let Some(transport) = request.headers.get("Transport") {
            let track_idx = self.setup_count;
            self.setup_count += 1;
            let rtp_channel = (track_idx * 2) as u8;

            if transport.contains("TCP") || transport.contains("interleaved") {
                self.tcp_interleaved = true;
                let transport_reply = format!(
                    "RTP/AVP/TCP;unicast;interleaved={}-{}",
                    rtp_channel,
                    rtp_channel + 1
                );
                if track_idx == 0 {
                    self.video_chan = rtp_channel;
                } else {
                    self.audio_chan = rtp_channel;
                }
                let response = RtspResponse::new(200, "OK")
                    .with_header("CSeq", &self.cseq.to_string())
                    .with_header("Session", &self.session_id)
                    .with_header("Transport", &transport_reply);
                self.send_response(&response).await?;
            } else if let Some((rtp_port, rtcp_port)) = Self::parse_udp_transport(transport) {
                let rtp_socket = UdpSocket::bind("0.0.0.0:0").await?;
                let rtcp_socket = UdpSocket::bind("0.0.0.0:0").await?;

                let rtp_local = rtp_socket.local_addr()?;
                let rtcp_local = rtcp_socket.local_addr()?;

                let client_addr: SocketAddr = format!("{}:{}", self.client_ip, rtp_port)
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Invalid client addr: {}", e))?;
                let client_rtcp_addr: SocketAddr = format!("{}:{}", self.client_ip, rtcp_port)
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Invalid client RTCP addr: {}", e))?;

                rtp_socket.connect(client_addr).await?;
                rtcp_socket.connect(client_rtcp_addr).await?;

                info!(
                    "RTSP UDP track{}: local rtp={} rtcp={} -> client rtp={} rtcp={}",
                    track_idx,
                    rtp_local.port(),
                    rtcp_local.port(),
                    rtp_port,
                    rtcp_port
                );

                if track_idx == 0 {
                    self.udp_video = Some(Arc::new(rtp_socket));
                } else {
                    self.udp_audio = Some(Arc::new(rtp_socket));
                }

                let transport_reply = format!(
                    "RTP/AVP/UDP;unicast;client_port={}-{};server_port={}-{}",
                    rtp_port,
                    rtcp_port,
                    rtp_local.port(),
                    rtcp_local.port()
                );
                let response = RtspResponse::new(200, "OK")
                    .with_header("CSeq", &self.cseq.to_string())
                    .with_header("Session", &self.session_id)
                    .with_header("Transport", &transport_reply);
                self.send_response(&response).await?;
            }
        } else {
            let response = RtspResponse::new(461, "Unsupported Transport")
                .with_header("CSeq", &self.cseq.to_string());
            self.send_response(&response).await?;
        }
        Ok(())
    }

    async fn handle_play(&mut self, request: &RtspRequest) -> anyhow::Result<()> {
        self.is_publisher = false;
        if self.app.is_empty() {
            self.parse_uri(&request.uri);
        }

        if !self.is_authenticated(request, "play").await {
            warn!(
                "RTSP play rejected (auth): {}/{}",
                self.app, self.stream_name
            );
            let resp = self.auth_reject(request).await;
            return self.send_response(&resp).await;
        }

        // If DESCRIBE was skipped, look up the source now.
        if self.source.is_none() {
            self.source = self
                .source_manager
                .get_or_pull("__defaultVhost__", &self.app, &self.stream_name)
                .await;
        }

        if self.source.is_none() && self.vod_asset.is_none() {
            self.vod_asset = self.open_vod().await;
        }

        if let Some(asset) = self.vod_asset.take() {
            return self.handle_vod_play(request, asset).await;
        }

        if self.source.is_none() {
            let response =
                RtspResponse::new(404, "Not Found").with_header("CSeq", &self.cseq.to_string());
            return self.send_response(&response).await;
        }

        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id);
        self.send_response(&response).await?;

        if let Some(ref source) = self.source {
            let subscriber = source
                .register_subscriber(format!("rtsp:{}", self.session_id))
                .await;
            let play_stop = Arc::new(Notify::new());
            if let Some(previous) = self.play_stop.replace(play_stop.clone()) {
                previous.notify_one();
            }
            let cached = {
                let cache = source.gop_cache.read().await;
                cache.get_latest_gop_frames()
            };
            let audio_clock_rate = {
                let info = source.info.read().await;
                info.tracks
                    .iter()
                    .find_map(|track| match track {
                        TrackInfo::Audio(audio) => Some(match audio.codec {
                            CodecId::Opus => 48_000,
                            CodecId::G711A | CodecId::G711U => 8_000,
                            CodecId::Mp3 | CodecId::MP2A => 90_000,
                            _ => audio.sample_rate.max(1),
                        }),
                        _ => None,
                    })
                    .unwrap_or(44_100)
            };

            let mut rx = source.subscribe();

            if self.tcp_interleaved {
                let stream = match self.stream.take() {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let handle = tokio::spawn(async move {
                    let _subscriber = subscriber;
                    let mut writer = stream;
                    let mut seq: u16 = 1;
                    let ssrc: u32 = rand::rng().random();
                    debug!(
                        "RTSP play send task started (tcp interleaved), cached={}",
                        cached.len()
                    );

                    for frame in &cached {
                        let _ = Self::send_interleaved_frame(
                            &mut writer,
                            frame,
                            &mut seq,
                            ssrc,
                            audio_clock_rate,
                        )
                        .await;
                    }

                    loop {
                        tokio::select! {
                            _ = play_stop.notified() => break,
                            result = rx.recv() => match result {
                                Ok(frame) => {
                                    if Self::send_interleaved_frame(
                                        &mut writer,
                                        &frame,
                                        &mut seq,
                                        ssrc,
                                        audio_clock_rate,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                    debug!("RTSP play send task ended (tcp interleaved)");
                });

                handle.await.ok();
            } else {
                let video_sock = match self.udp_video.take() {
                    Some(s) => s,
                    None => return Ok(()),
                };
                let audio_sock = self.udp_audio.take();
                tokio::spawn(async move {
                    let _subscriber = subscriber;
                    let mut video_seq: u16 = 1;
                    let mut audio_seq: u16 = 1;
                    let video_ssrc: u32 = rand::rng().random();
                    let audio_ssrc: u32 = rand::rng().random();

                    for frame in &cached {
                        match frame.frame_type {
                            zlmediakit_core::media_frame::FrameType::Video => {
                                let _ = Self::send_udp_frame(
                                    &video_sock,
                                    frame,
                                    &mut video_seq,
                                    video_ssrc,
                                    audio_clock_rate,
                                )
                                .await;
                            }
                            zlmediakit_core::media_frame::FrameType::Audio => {
                                if let Some(ref sock) = audio_sock {
                                    let _ = Self::send_udp_frame(
                                        sock,
                                        frame,
                                        &mut audio_seq,
                                        audio_ssrc,
                                        audio_clock_rate,
                                    )
                                    .await;
                                }
                            }
                            _ => {}
                        }
                    }

                    loop {
                        tokio::select! {
                            _ = play_stop.notified() => break,
                            result = rx.recv() => match result {
                                Ok(frame) => match frame.frame_type {
                                    zlmediakit_core::media_frame::FrameType::Video => {
                                        let _ = Self::send_udp_frame(
                                            &video_sock,
                                            &frame,
                                            &mut video_seq,
                                            video_ssrc,
                                            audio_clock_rate,
                                        )
                                        .await;
                                    }
                                    zlmediakit_core::media_frame::FrameType::Audio => {
                                        if let Some(ref sock) = audio_sock {
                                            let _ = Self::send_udp_frame(
                                                sock,
                                                &frame,
                                                &mut audio_seq,
                                                audio_ssrc,
                                                audio_clock_rate,
                                            )
                                            .await;
                                        }
                                    }
                                    _ => {}
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                });
            }
        }

        Ok(())
    }

    async fn open_vod(&self) -> Option<VodAsset> {
        let library = self.vod_library.as_ref()?;
        match library.open(&self.app, &self.stream_name).await {
            Ok(asset) => asset,
            Err(error) => {
                warn!(
                    "RTSP VOD open failed for {}/{}: {error:#}",
                    self.app, self.stream_name
                );
                None
            }
        }
    }

    async fn handle_vod_play(
        &mut self,
        request: &RtspRequest,
        asset: VodAsset,
    ) -> anyhow::Result<()> {
        let (requested_start_ms, requested_end_ms) = parse_npt_range(request);
        let playback = asset.playback(requested_start_ms);
        let actual_start_ms = playback.start_ms;
        let limit_ms = requested_end_ms.map(|end| end.saturating_sub(actual_start_ms));
        let audio_clock_rate = asset
            .media_info()
            .tracks
            .iter()
            .find_map(|track| match track {
                TrackInfo::Audio(audio) => Some(match audio.codec {
                    CodecId::Opus => 48_000,
                    CodecId::G711A | CodecId::G711U => 8_000,
                    CodecId::Mp3 | CodecId::MP2A => 90_000,
                    _ => audio.sample_rate.max(1),
                }),
                _ => None,
            })
            .unwrap_or(44_100);
        let range = format!("npt={:.3}-", actual_start_ms as f64 / 1000.0);
        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id)
            .with_header("Range", &range);
        self.send_response(&response).await?;

        if self.tcp_interleaved {
            let mut writer = match self.stream.take() {
                Some(stream) => stream,
                None => return Ok(()),
            };
            let frames = playback.frames;
            tokio::spawn(async move {
                let pacer = VodPacer::new();
                let mut sequence = 1u16;
                let ssrc = rand::rng().random();
                for frame in frames {
                    if !frame.config_frame {
                        if limit_ms.is_some_and(|limit| u64::from(frame.timestamp) > limit) {
                            break;
                        }
                        pacer.wait(frame.timestamp).await;
                    }
                    if Self::send_interleaved_frame(
                        &mut writer,
                        &frame,
                        &mut sequence,
                        ssrc,
                        audio_clock_rate,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                debug!("RTSP VOD TCP playback ended");
            });
        } else {
            let video_socket = match self.udp_video.take() {
                Some(socket) => socket,
                None => return Ok(()),
            };
            let audio_socket = self.udp_audio.take();
            let frames = playback.frames;
            tokio::spawn(async move {
                let pacer = VodPacer::new();
                let mut video_sequence = 1u16;
                let mut audio_sequence = 1u16;
                let video_ssrc = rand::rng().random();
                let audio_ssrc = rand::rng().random();
                for frame in frames {
                    if !frame.config_frame {
                        if limit_ms.is_some_and(|limit| u64::from(frame.timestamp) > limit) {
                            break;
                        }
                        pacer.wait(frame.timestamp).await;
                    }
                    let result = match frame.frame_type {
                        FrameType::Video => {
                            Self::send_udp_frame(
                                &video_socket,
                                &frame,
                                &mut video_sequence,
                                video_ssrc,
                                audio_clock_rate,
                            )
                            .await
                        }
                        FrameType::Audio => match &audio_socket {
                            Some(socket) => {
                                Self::send_udp_frame(
                                    socket,
                                    &frame,
                                    &mut audio_sequence,
                                    audio_ssrc,
                                    audio_clock_rate,
                                )
                                .await
                            }
                            None => Ok(()),
                        },
                        FrameType::Metadata => Ok(()),
                    };
                    if result.is_err() {
                        break;
                    }
                }
                debug!("RTSP VOD UDP playback ended");
            });
        }
        info!(
            "RTSP VOD play started: {}/{} at {} ms",
            self.app, self.stream_name, actual_start_ms
        );
        Ok(())
    }

    /// Returns `true` when the request is authorized for `action`
    /// (`"play"` or `"pub"`). It first tries the legacy `sign` query
    /// parameter, then a `Digest` or `Basic` `Authorization` header when a
    /// realm has already been negotiated via `on_rtsp_realm`.
    async fn is_authenticated(&self, request: &RtspRequest, action: &str) -> bool {
        if self.auth.check(
            "__defaultVhost__",
            &self.app,
            &self.stream_name,
            action,
            &self.stream_sign,
        ) {
            return true;
        }

        let header = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
            .map(|(_, v)| v.clone());
        if let Some(header) = header {
            // Basic auth (RFC 7617) does not need a realm to validate.
            if header.to_ascii_lowercase().starts_with("basic ") {
                return self.auth.check_basic(&header);
            }
            if let Some(realm) = &self.realm {
                return self
                    .auth
                    .check_digest(realm, &request.method, &request.uri, &header);
            }
        }
        false
    }

    /// Builds the 401 response for an unauthorized request. When a hook is
    /// configured and `on_rtsp_realm` yields a realm, the response carries a
    /// `WWW-Authenticate` challenge advertising both `Digest` and `Basic`
    /// (RFC 7235) so the client can retry with either scheme. The negotiated
    /// realm is cached on the session for subsequent requests.
    async fn auth_reject(&mut self, request: &RtspRequest) -> RtspResponse {
        if self.realm.is_none() {
            if let Some(hook) = &self.hook {
                if let Some(realm) = hook
                    .on_rtsp_realm(
                        "__defaultVhost__",
                        &self.app,
                        &self.stream_name,
                        &self.client_ip,
                    )
                    .await
                {
                    if !realm.is_empty() {
                        self.realm = Some(realm);
                    }
                }
            }
            // Fall back to a configured static realm so a Digest challenge can
            // still be issued when no hook (or an empty hook response) is used.
            if self.realm.is_none() {
                if let Some(realm) = &self.auth.default_realm {
                    if !realm.is_empty() {
                        self.realm = Some(realm.clone());
                    }
                }
            }
        }

        let mut response =
            RtspResponse::new(401, "Unauthorized").with_header("CSeq", &self.cseq.to_string());
        if let Some(realm) = &self.realm {
            response = response.with_header(
                "WWW-Authenticate",
                &format!(
                    "Basic realm=\"{}\", Digest realm=\"{}\", nonce=\"{}\"",
                    realm, realm, self.nonce
                ),
            );
        }
        let _ = request;
        response
    }

    async fn handle_announce(&mut self, request: &RtspRequest) -> anyhow::Result<()> {
        self.is_publisher = true;
        self.parse_uri(&request.uri);

        if !self.is_authenticated(request, "pub").await {
            warn!(
                "RTSP publish rejected (auth): {}/{}",
                self.app, self.stream_name
            );
            let resp = self.auth_reject(request).await;
            return self.send_response(&resp).await;
        }

        // Parse the SDP body to learn payload types, codecs and decoder config.
        let sdp = parse_sdp(&request.body);
        self.video_pt = sdp.video_pt;
        self.audio_pt = sdp.audio_pt;
        self.audio_sample_rate = sdp.audio_sample_rate;
        self.video_codec = sdp.video_codec;
        self.sps = sdp.sps;
        self.pps = sdp.pps;
        self.vps = sdp.vps;
        self.audio_config = sdp.audio_config;

        self.source = Some(self.source_manager.get_or_create(
            "__defaultVhost__",
            &self.app,
            &self.stream_name,
        ));

        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id);
        self.send_response(&response).await
    }

    async fn handle_record(&mut self) -> anyhow::Result<()> {
        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id);
        self.send_response(&response).await?;

        if !self.is_publisher {
            return Ok(());
        }

        let source = match self.source.clone() {
            Some(s) => s,
            None => return Ok(()),
        };

        // Populate the source track info so other protocols (FLV/HLS) can expose
        // correct metadata for this published stream.
        {
            let mut info = source.info.write().await;
            info.tracks.clear();
            info.tracks.push(TrackInfo::Video(VideoInfo {
                codec: self.video_codec,
                width: 1280,
                height: 720,
                fps: 25.0,
                key_frame: false,
            }));
            if self.audio_config.is_some() {
                info.tracks.push(TrackInfo::Audio(AudioInfo {
                    codec: CodecId::AAC,
                    sample_rate: self.audio_sample_rate,
                    channels: 1,
                    bits_per_sample: 16,
                }));
            }
        }

        let (sps, pps, audio_config) = (
            self.sps.clone(),
            self.pps.clone(),
            self.audio_config.clone(),
        );

        if self.tcp_interleaved {
            // TCP-interleaved RTP is demuxed inside the main read loop.
            let depak = RtpDepacketizer::new(
                source,
                self.video_pt,
                self.audio_pt,
                self.video_chan,
                self.audio_chan,
                self.audio_sample_rate,
                None,
                self.video_codec,
                sps,
                pps,
                self.vps.clone(),
                audio_config,
            );
            self.depak = Some(depak);
        } else {
            // UDP RTP: spawn per-track receive tasks.
            if let Some(sock) = self.udp_video.take() {
                let mut depak = RtpDepacketizer::new(
                    source.clone(),
                    self.video_pt,
                    self.audio_pt,
                    self.video_chan,
                    self.audio_chan,
                    self.audio_sample_rate,
                    Some(FrameType::Video),
                    self.video_codec,
                    sps.clone(),
                    pps.clone(),
                    self.vps.clone(),
                    audio_config.clone(),
                );
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    while let Ok(n) = sock.recv(&mut buf).await {
                        depak.handle_rtp(0, &buf[..n]).await;
                    }
                });
            }
            if let Some(sock) = self.udp_audio.take() {
                let mut depak = RtpDepacketizer::new(
                    source.clone(),
                    self.video_pt,
                    self.audio_pt,
                    self.video_chan,
                    self.audio_chan,
                    self.audio_sample_rate,
                    Some(FrameType::Audio),
                    self.video_codec,
                    sps,
                    pps,
                    self.vps.clone(),
                    audio_config,
                );
                tokio::spawn(async move {
                    let mut buf = [0u8; 65536];
                    while let Ok(n) = sock.recv(&mut buf).await {
                        depak.handle_rtp(0, &buf[..n]).await;
                    }
                });
            }
        }

        info!(
            "RTSP publish (RECORD) started: {}/{} (tcp={})",
            self.app, self.stream_name, self.tcp_interleaved
        );

        // Notify the recorder supervisor (and any future hooks) that a stream
        // has started publishing.
        self.event_bus.publish(Event::StreamPublish {
            source_id: format!("__defaultVhost__/{}/{}", self.app, self.stream_name),
            vhost: "__defaultVhost__".to_string(),
            app: self.app.clone(),
            stream: self.stream_name.clone(),
        });
        Ok(())
    }

    async fn handle_teardown(&mut self) -> anyhow::Result<()> {
        self.cleanup().await;
        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id);
        self.send_response(&response).await
    }

    async fn send_response(&mut self, response: &RtspResponse) -> anyhow::Result<()> {
        if let Some(ref mut stream) = self.stream {
            let data = response.serialize();
            stream.write_all(&data).await?;
        }
        Ok(())
    }

    fn parse_uri(&mut self, uri: &str) {
        let stripped = uri
            .strip_prefix("rtsp://")
            .or_else(|| uri.strip_prefix("rtsps://"))
            .unwrap_or(uri);
        let (path, query) = match stripped.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (stripped, None),
        };
        let path = if path.starts_with('/') {
            path.trim_start_matches('/')
        } else {
            path.split_once('/').map(|x| x.1).unwrap_or("")
        };
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        self.app = parts.first().map_or("live", |v| v).to_string();
        self.stream_name = parts.get(1).map_or("stream", |v| v).to_string();
        self.stream_sign = query
            .map(|q| {
                q.split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(k, _)| *k == "sign")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
    }

    fn parse_udp_transport(transport: &str) -> Option<(u16, u16)> {
        let port_str = transport.split(';').find(|p| p.contains("client_port"))?;
        let ports: Vec<&str> = port_str.split('=').nth(1)?.split('-').collect();
        if ports.len() >= 2 {
            let rtp = ports[0].parse().ok()?;
            let rtcp = ports[1].parse().ok()?;
            Some((rtp, rtcp))
        } else {
            None
        }
    }

    fn generate_sdp(
        &self,
        video_codec: CodecId,
        hevc_sprop: Option<(String, String, String)>,
        audio: Option<&AudioInfo>,
    ) -> String {
        let (video_rtpmap, video_fmtp) = match video_codec {
            CodecId::H265 => {
                if let Some((vps, sps, pps)) = hevc_sprop {
                    (
                        "H265/90000".to_string(),
                        format!(
                            "a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
                            vps, sps, pps
                        ),
                    )
                } else {
                    ("H265/90000".to_string(), String::new())
                }
            }
            _ => (
                "H264/90000".to_string(),
                "a=fmtp:96 packetization-mode=1;profile-level-id=42C01E\r\n".to_string(),
            ),
        };
        let mut sdp = format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 127.0.0.1\r\n\
             s=zlmediakit-rs\r\n\
             c=IN IP4 0.0.0.0\r\n\
             t=0 0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 {}\r\n\
             {}\
             a=control:track1\r\n",
            video_rtpmap, video_fmtp
        );
        if let Some(audio) = audio {
            sdp.push_str(&generate_audio_sdp(audio));
        }
        sdp
    }

    fn make_rtp_header(seq: u16, timestamp: u32, ssrc: u32, pt: u8, marker: bool) -> [u8; 12] {
        let mut hdr = [0u8; 12];
        hdr[0] = 0x80;
        hdr[1] = if marker { 0x80 | pt } else { pt };
        hdr[2] = (seq >> 8) as u8;
        hdr[3] = seq as u8;
        hdr[4] = (timestamp >> 24) as u8;
        hdr[5] = (timestamp >> 16) as u8;
        hdr[6] = (timestamp >> 8) as u8;
        hdr[7] = timestamp as u8;
        hdr[8] = (ssrc >> 24) as u8;
        hdr[9] = (ssrc >> 16) as u8;
        hdr[10] = (ssrc >> 8) as u8;
        hdr[11] = ssrc as u8;
        hdr
    }

    fn packetize_h264(data: &[u8], timestamp: u32, ssrc: u32, seq: &mut u16) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();

        if data.len() < 5 {
            return packets;
        }

        let _frame_type = (data[0] >> 4) & 0x0F;
        let avc_packet_type = data[1] & 0x0F;

        match avc_packet_type {
            0 => {
                // AVCDecoderConfigurationRecord: starts at data[5]
                // data[5]=version, [6]=profile, [7]=compat, [8]=level,
                // data[9]=lengthSizeMinusOne, data[10]=numSPS
                if data.len() >= 11 {
                    let num_sps = (data[10] & 0x1F) as usize;
                    let mut pos = 11;
                    for _ in 0..num_sps {
                        if pos + 2 > data.len() {
                            break;
                        }
                        let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                        pos += 2;
                        if pos + nalu_len > data.len() {
                            break;
                        }
                        let nalu = &data[pos..pos + nalu_len];
                        let rtp_ts = timestamp * 90;
                        let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, true);
                        *seq = seq.wrapping_add(1);
                        let mut pkt = Vec::with_capacity(12 + nalu.len());
                        pkt.extend_from_slice(&hdr);
                        pkt.extend_from_slice(nalu);
                        packets.push(pkt);
                        pos += nalu_len;
                    }
                    // PPS
                    if pos < data.len() {
                        let num_pps = data[pos] as usize;
                        pos += 1;
                        for _ in 0..num_pps {
                            if pos + 2 > data.len() {
                                break;
                            }
                            let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                            pos += 2;
                            if pos + nalu_len > data.len() {
                                break;
                            }
                            let nalu = &data[pos..pos + nalu_len];
                            let rtp_ts = timestamp * 90;
                            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, true);
                            *seq = seq.wrapping_add(1);
                            let mut pkt = Vec::with_capacity(12 + nalu.len());
                            pkt.extend_from_slice(&hdr);
                            pkt.extend_from_slice(nalu);
                            packets.push(pkt);
                            pos += nalu_len;
                        }
                    }
                }
            }
            1 => {
                let rtp_ts = timestamp * 90;
                let mut pos = 5;
                let mtu = 1400;

                while pos + 4 <= data.len() {
                    let nalu_len = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + nalu_len > data.len() {
                        break;
                    }
                    let nalu = &data[pos..pos + nalu_len];
                    pos += nalu_len;

                    if nalu.len() <= mtu {
                        let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, true);
                        *seq = seq.wrapping_add(1);
                        let mut pkt = Vec::with_capacity(12 + nalu.len());
                        pkt.extend_from_slice(&hdr);
                        pkt.extend_from_slice(nalu);
                        packets.push(pkt);
                    } else {
                        let nal_header = nalu[0];
                        let nal_type = nal_header & 0x1F;
                        let nri = nal_header & 0x60;
                        let fu_indicator = nri | 28;

                        let mut pos2 = 1;
                        let mut first = true;
                        while pos2 < nalu.len() {
                            let chunk = std::cmp::min(mtu - 2, nalu.len() - pos2);
                            let end = pos2 + chunk >= nalu.len();
                            let fu_header = if first {
                                0x80 | nal_type
                            } else if end {
                                0x40 | nal_type
                            } else {
                                nal_type
                            };

                            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, end);
                            *seq = seq.wrapping_add(1);
                            let mut pkt = Vec::with_capacity(12 + 2 + chunk);
                            pkt.extend_from_slice(&hdr);
                            pkt.push(fu_indicator);
                            pkt.push(fu_header);
                            pkt.extend_from_slice(&nalu[pos2..pos2 + chunk]);
                            packets.push(pkt);

                            pos2 += chunk;
                            first = false;
                        }
                    }
                }
            }
            _ => {}
        }

        packets
    }

    fn packetize_video(
        frame: &MediaFrame,
        timestamp: u32,
        ssrc: u32,
        seq: &mut u16,
    ) -> Vec<Vec<u8>> {
        let Ok(data) = flv_payload(frame) else {
            return Vec::new();
        };
        match frame.codec {
            CodecId::H265 => {
                let mut normalized = frame.clone();
                normalized.data = data;
                normalized.payload_format = PayloadFormat::Flv;
                Self::packetize_hevc(&normalized, timestamp, ssrc, seq)
            }
            CodecId::H264 => Self::packetize_h264(&data, timestamp, ssrc, seq),
            _ => Vec::new(),
        }
    }

    fn packetize_hevc(
        frame: &MediaFrame,
        timestamp: u32,
        ssrc: u32,
        seq: &mut u16,
    ) -> Vec<Vec<u8>> {
        let data = &frame.data;
        let mut packets = Vec::new();
        if data.len() < 5 {
            return packets;
        }
        let packet_type = data[1] & 0x0F;
        let rtp_ts = timestamp * 90;

        match packet_type {
            0 => {
                // HEVCDecoderConfigurationRecord: emit VPS/SPS/PPS as
                // individual single-NALU RTP packets (in-band parameter sets).
                let (vps, sps, pps) = extract_hevc_nalus(data);
                for nalu in [vps, sps, pps].iter().flatten() {
                    let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, true);
                    *seq = seq.wrapping_add(1);
                    let mut pkt = Vec::with_capacity(12 + nalu.len());
                    pkt.extend_from_slice(&hdr);
                    pkt.extend_from_slice(nalu);
                    packets.push(pkt);
                }
            }
            1 => {
                let mut pos = 5;
                let mtu = 1400;
                while pos + 4 <= data.len() {
                    let nalu_len = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + nalu_len > data.len() {
                        break;
                    }
                    let nalu = &data[pos..pos + nalu_len];
                    pos += nalu_len;
                    if nalu.len() < 2 {
                        continue;
                    }
                    if nalu.len() <= mtu {
                        let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, true);
                        *seq = seq.wrapping_add(1);
                        let mut pkt = Vec::with_capacity(12 + nalu.len());
                        pkt.extend_from_slice(&hdr);
                        pkt.extend_from_slice(nalu);
                        packets.push(pkt);
                    } else {
                        // RFC 7798 HEVC FU fragmentation: 2-byte PayloadHdr
                        // (Type=49) + 1-byte FU header + fragment data.
                        let nal_header0 = nalu[0];
                        let nal_header1 = nalu[1];
                        // HEVC NAL type lives in the upper 6 bits of byte 0.
                        let nal_type = (nal_header0 >> 1) & 0x3F;
                        // PayloadHdr byte0: keep F bit + LayerId high bit, Type=49.
                        let payload_hdr0 = (nal_header0 & 0x81) | (49 << 1);
                        let mut pos2 = 2;
                        let mut first = true;
                        while pos2 < nalu.len() {
                            let chunk = std::cmp::min(mtu - 3, nalu.len() - pos2);
                            let end = pos2 + chunk >= nalu.len();
                            let mut fu_header = nal_type;
                            if first {
                                fu_header |= 0x80;
                            }
                            if end {
                                fu_header |= 0x40;
                            }
                            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, 96, end);
                            *seq = seq.wrapping_add(1);
                            let mut pkt = Vec::with_capacity(12 + 3 + chunk);
                            pkt.extend_from_slice(&hdr);
                            pkt.push(payload_hdr0);
                            pkt.push(nal_header1);
                            pkt.push(fu_header);
                            pkt.extend_from_slice(&nalu[pos2..pos2 + chunk]);
                            packets.push(pkt);
                            pos2 += chunk;
                            first = false;
                        }
                    }
                }
            }
            _ => {}
        }

        packets
    }

    fn packetize_audio(
        frame: &MediaFrame,
        timestamp: u32,
        ssrc: u32,
        seq: &mut u16,
        clock_rate: u32,
    ) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        if frame.config_frame {
            return packets;
        }
        let (pt, audio_data, aac, mpa) = match frame.codec {
            CodecId::AAC => {
                let Ok(data) = aac_raw_payload(frame) else {
                    return packets;
                };
                (97, data, true, false)
            }
            CodecId::Opus => (97, frame.data.clone(), false, false),
            CodecId::G711U => (0, raw_audio_payload(frame), false, false),
            CodecId::G711A => (8, raw_audio_payload(frame), false, false),
            CodecId::Mp3 | CodecId::MP2A => (14, raw_audio_payload(frame), false, true),
            CodecId::L16 => (97, raw_audio_payload(frame), false, false),
            _ => return packets,
        };
        if audio_data.is_empty() {
            return packets;
        }
        let rtp_ts = ((timestamp as u64 * clock_rate.max(1) as u64) / 1000) as u32;

        if aac {
            // RFC 3640 format with AU headers
            let au_size = audio_data.len();
            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, pt, true);
            *seq = seq.wrapping_add(1);
            let mut pkt = Vec::with_capacity(12 + 2 + 2 + au_size);
            pkt.extend_from_slice(&hdr);
            pkt.put_u16(16);
            let au_header = ((au_size as u16) << 3) & 0xFFF8;
            pkt.put_u16(au_header);
            pkt.extend_from_slice(&audio_data);
            packets.push(pkt);
        } else {
            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, pt, true);
            *seq = seq.wrapping_add(1);
            let mut pkt = Vec::with_capacity(12 + if mpa { 4 } else { 0 } + audio_data.len());
            pkt.extend_from_slice(&hdr);
            if mpa {
                // RFC 2250 MPEG audio header: MBZ + fragment offset.
                pkt.extend_from_slice(&[0, 0, 0, 0]);
            }
            pkt.extend_from_slice(&audio_data);
            packets.push(pkt);
        }

        packets
    }

    async fn send_interleaved_rtp(
        stream: &mut TransportStream,
        channel: u8,
        rtp_packets: Vec<Vec<u8>>,
    ) -> anyhow::Result<()> {
        for pkt in rtp_packets {
            let mut header = BytesMut::with_capacity(4);
            header.put_u8(b'$');
            header.put_u8(channel);
            header.put_u16(pkt.len() as u16);
            stream.write_all(&header).await?;
            stream.write_all(&pkt).await?;
        }
        Ok(())
    }

    async fn send_udp_frame(
        socket: &UdpSocket,
        frame: &MediaFrame,
        seq: &mut u16,
        ssrc: u32,
        audio_clock_rate: u32,
    ) -> anyhow::Result<()> {
        let packets = match frame.frame_type {
            zlmediakit_core::media_frame::FrameType::Video => {
                Self::packetize_video(frame, frame.timestamp, ssrc, seq)
            }
            zlmediakit_core::media_frame::FrameType::Audio => {
                Self::packetize_audio(frame, frame.timestamp, ssrc, seq, audio_clock_rate)
            }
            _ => return Ok(()),
        };

        for pkt in packets {
            let _ = socket.send(&pkt).await;
        }
        Ok(())
    }

    async fn send_interleaved_frame(
        stream: &mut TransportStream,
        frame: &MediaFrame,
        seq: &mut u16,
        ssrc: u32,
        audio_clock_rate: u32,
    ) -> anyhow::Result<()> {
        let packets = match frame.frame_type {
            zlmediakit_core::media_frame::FrameType::Video => {
                Self::packetize_video(frame, frame.timestamp, ssrc, seq)
            }
            zlmediakit_core::media_frame::FrameType::Audio => {
                Self::packetize_audio(frame, frame.timestamp, ssrc, seq, audio_clock_rate)
            }
            _ => return Ok(()),
        };

        let channel = if frame.frame_type == zlmediakit_core::media_frame::FrameType::Video {
            0
        } else {
            2
        };

        Self::send_interleaved_rtp(stream, channel, packets).await
    }

    async fn cleanup(&mut self) {
        info!("RTSP session closing: {}/{}", self.app, self.stream_name);
        if self.is_publisher {
            self.event_bus.publish(Event::StreamUnPublish {
                source_id: format!("__defaultVhost__/{}/{}", self.app, self.stream_name),
                vhost: "__defaultVhost__".to_string(),
                app: self.app.clone(),
                stream: self.stream_name.clone(),
            });
            self.source_manager
                .close_stream("__defaultVhost__", &self.app, &self.stream_name);
        }
        self.source = None;
        if let Some(stop) = self.play_stop.take() {
            stop.notify_one();
        }
        self.udp_video = None;
        self.udp_audio = None;
        self.depak = None;
    }
}

fn raw_audio_payload(frame: &MediaFrame) -> Bytes {
    let is_flv = frame.payload_format == PayloadFormat::Flv
        || (frame.payload_format == PayloadFormat::Unknown
            && frame
                .data
                .first()
                .is_some_and(|header| matches!(header >> 4, 2 | 7 | 8)));
    if is_flv && !frame.data.is_empty() {
        frame.data.slice(1..)
    } else {
        frame.data.clone()
    }
}

fn generate_audio_sdp(audio: &AudioInfo) -> String {
    let channels = audio.channels.max(1);
    match audio.codec {
        CodecId::AAC => format!(
            "m=audio 0 RTP/AVP 97\r\n\
             a=rtpmap:97 MPEG4-GENERIC/{}/{}\r\n\
             a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3\r\n\
             a=control:track2\r\n",
            audio.sample_rate, channels
        ),
        CodecId::Opus => format!(
            "m=audio 0 RTP/AVP 97\r\n\
             a=rtpmap:97 opus/48000/{}\r\n\
             a=fmtp:97 minptime=10;useinbandfec=1\r\n\
             a=control:track2\r\n",
            channels
        ),
        CodecId::G711A => "m=audio 0 RTP/AVP 8\r\n\
             a=rtpmap:8 PCMA/8000/1\r\n\
             a=control:track2\r\n"
            .to_string(),
        CodecId::G711U => "m=audio 0 RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000/1\r\n\
             a=control:track2\r\n"
            .to_string(),
        CodecId::Mp3 | CodecId::MP2A => "m=audio 0 RTP/AVP 14\r\n\
             a=rtpmap:14 MPA/90000\r\n\
             a=control:track2\r\n"
            .to_string(),
        CodecId::L16 => format!(
            "m=audio 0 RTP/AVP 97\r\n\
             a=rtpmap:97 L16/{}/{}\r\n\
             a=control:track2\r\n",
            audio.sample_rate, channels
        ),
        _ => String::new(),
    }
}

// === RTSP push: RTP depacketization into FLV/AVCC MediaFrames ===

pub(crate) struct RtpDepacketizer {
    source: Arc<MediaSource>,
    video_pt: u8,
    audio_pt: u8,
    video_chan: u8,
    audio_chan: u8,
    audio_sample_rate: u32,
    force_media: Option<FrameType>,
    video_codec: CodecId,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    vps: Option<Vec<u8>>,
    audio_config: Option<Vec<u8>>,
    config_sent: bool,
    audio_config_sent: bool,
    pending_nalus: Vec<Vec<u8>>,
    fu_buf: Vec<u8>,
    fu_active: bool,
    pending_video_ts: u32,
    video_key: bool,
}

impl RtpDepacketizer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source: Arc<MediaSource>,
        video_pt: u8,
        audio_pt: u8,
        video_chan: u8,
        audio_chan: u8,
        audio_sample_rate: u32,
        force_media: Option<FrameType>,
        video_codec: CodecId,
        sps: Option<Vec<u8>>,
        pps: Option<Vec<u8>>,
        vps: Option<Vec<u8>>,
        audio_config: Option<Vec<u8>>,
    ) -> Self {
        Self {
            source,
            video_pt,
            audio_pt,
            video_chan,
            audio_chan,
            audio_sample_rate,
            force_media,
            video_codec,
            sps,
            pps,
            vps,
            audio_config,
            config_sent: false,
            audio_config_sent: false,
            pending_nalus: Vec::new(),
            fu_buf: Vec::new(),
            fu_active: false,
            pending_video_ts: 0,
            video_key: false,
        }
    }

    pub(crate) async fn handle_rtp(&mut self, channel: u8, payload: &[u8]) {
        if payload.len() < 12 {
            return;
        }
        let pt = payload[1] & 0x7F;
        let media = if let Some(m) = self.force_media {
            m
        } else if pt == self.video_pt || channel == self.video_chan {
            FrameType::Video
        } else if pt == self.audio_pt || channel == self.audio_chan {
            FrameType::Audio
        } else {
            return;
        };
        let cc = (payload[0] & 0x0F) as usize;
        let header_len = 12 + 4 * cc;
        if payload.len() < header_len + 1 {
            return;
        }
        let marker = (payload[1] & 0x80) != 0;
        let ts = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let data = &payload[header_len..];
        match media {
            FrameType::Video => self.handle_video_rtp(data, marker, ts).await,
            FrameType::Audio => self.handle_audio_rtp(data, ts).await,
            _ => {}
        }
    }

    async fn handle_video_rtp(&mut self, data: &[u8], marker: bool, ts: u32) {
        if data.is_empty() {
            return;
        }
        self.pending_video_ts = ts;
        if self.video_codec == CodecId::H265 {
            self.handle_hevc_video_rtp(data);
            if marker {
                self.emit_video_frame().await;
            }
            return;
        }
        let nalu_type = data[0] & 0x1F;
        match nalu_type {
            1..=23 => {
                // Single NALU
                let nalu = data.to_vec();
                self.observe_nalu(&nalu);
                self.pending_nalus.push(nalu);
            }
            24 => {
                // STAP-A: multiple NALUs in one packet
                let mut pos = 1;
                while pos + 2 <= data.len() {
                    let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + len > data.len() {
                        break;
                    }
                    let nalu = data[pos..pos + len].to_vec();
                    pos += len;
                    self.observe_nalu(&nalu);
                    self.pending_nalus.push(nalu);
                }
            }
            28 => {
                // FU-A: fragmented NALU
                if data.len() < 2 {
                    return;
                }
                let fu_indicator = data[0];
                let fu_header = data[1];
                let start = (fu_header & 0x80) != 0;
                let end = (fu_header & 0x40) != 0;
                let nal_type = fu_header & 0x1F;
                let reconstructed = (fu_indicator & 0xE0) | nal_type;
                if start {
                    self.fu_buf.clear();
                    self.fu_buf.push(reconstructed);
                    self.fu_active = true;
                }
                if !self.fu_active {
                    return;
                }
                self.fu_buf.extend_from_slice(&data[2..]);
                if end {
                    let nalu = std::mem::take(&mut self.fu_buf);
                    self.fu_active = false;
                    if nal_type == 5 {
                        self.video_key = true;
                    }
                    self.pending_nalus.push(nalu);
                }
            }
            _ => {}
        }
        if marker {
            self.emit_video_frame().await;
        }
    }

    /// Depacketizes HEVC RTP payloads (RFC 7798). The HEVC payload header is
    /// two bytes; the NAL unit type lives in the top 6 bits of the first byte.
    fn handle_hevc_video_rtp(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let nal_type = (data[0] >> 1) & 0x3F;
        match nal_type {
            48 => {
                // AP (aggregation packet): each NALU is prefixed with a 2-byte
                // length. Skip the 2-byte payload header first.
                let mut pos = 2;
                while pos + 2 <= data.len() {
                    let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + len > data.len() {
                        break;
                    }
                    let nalu = data[pos..pos + len].to_vec();
                    pos += len;
                    self.observe_hevc_nalu(&nalu);
                    self.pending_nalus.push(nalu);
                }
            }
            49 => {
                // FU-A: 2-byte payload header + 1-byte FU header + fragment.
                if data.len() < 3 {
                    return;
                }
                let fu_header = data[2];
                let start = (fu_header & 0x80) != 0;
                let end = (fu_header & 0x40) != 0;
                let fu_type = fu_header & 0x3F;
                // Reconstructed NAL unit header (2 bytes).
                let reconstructed0 = (data[0] & 0x81) | (fu_type << 1);
                let reconstructed1 = data[1];
                if start {
                    self.fu_buf.clear();
                    self.fu_buf.push(reconstructed0);
                    self.fu_buf.push(reconstructed1);
                    self.fu_active = true;
                }
                if !self.fu_active {
                    return;
                }
                self.fu_buf.extend_from_slice(&data[3..]);
                if end {
                    let nalu = std::mem::take(&mut self.fu_buf);
                    self.fu_active = false;
                    self.observe_hevc_nalu(&nalu);
                    self.pending_nalus.push(nalu);
                }
            }
            _ => {
                // Single NALU unit (including VPS/SPS/PPS/IDR).
                let nalu = data.to_vec();
                self.observe_hevc_nalu(&nalu);
                self.pending_nalus.push(nalu);
            }
        }
    }

    fn observe_nalu(&mut self, nalu: &[u8]) {
        if nalu.is_empty() {
            return;
        }
        let t = nalu[0] & 0x1F;
        if t == 5 {
            self.video_key = true;
        } else if t == 7 {
            self.sps = Some(nalu.to_vec());
        } else if t == 8 {
            self.pps = Some(nalu.to_vec());
        }
    }

    fn observe_hevc_nalu(&mut self, nalu: &[u8]) {
        if nalu.len() < 2 {
            return;
        }
        let t = (nalu[0] >> 1) & 0x3F;
        match t {
            32 => self.vps = Some(nalu.to_vec()),
            33 => self.sps = Some(nalu.to_vec()),
            34 => self.pps = Some(nalu.to_vec()),
            19..=21 => self.video_key = true,
            _ => {}
        }
    }

    async fn emit_video_frame(&mut self) {
        if self.pending_nalus.is_empty() {
            return;
        }
        let ts = self.pending_video_ts;
        let ms = ts / 90;

        if self.video_codec == CodecId::H265 {
            if !self.config_sent {
                if let (Some(vps), Some(sps), Some(pps)) =
                    (self.vps.clone(), self.sps.clone(), self.pps.clone())
                {
                    let mut frame_data = Vec::with_capacity(5 + 64);
                    frame_data.push(0x1C); // key frame + HEVC codec id (12)
                    frame_data.push(0x00); // packet type = HEVC config record
                    frame_data.extend_from_slice(&[0, 0, 0]); // composition time
                    frame_data.extend_from_slice(&build_hevc_config_record(&vps, &sps, &pps));
                    let mut f = MediaFrame::new_video(
                        0,
                        CodecId::H265,
                        ms,
                        ms as u64,
                        ms as u64,
                        Bytes::from(frame_data),
                        false,
                    )
                    .with_payload_format(PayloadFormat::Flv);
                    f.config_frame = true;
                    let _ = self.source.publish_and_cache(f).await;
                    self.config_sent = true;
                }
            }

            let mut frame_nalus = Vec::new();
            for n in &self.pending_nalus {
                let t = (n[0] >> 1) & 0x3F;
                if t == 32 || t == 33 || t == 34 {
                    continue;
                }
                frame_nalus.push(n.clone());
            }
            self.pending_nalus.clear();
            if frame_nalus.is_empty() {
                self.video_key = false;
                return;
            }

            let mut frame_data = Vec::new();
            frame_data.push(if self.video_key { 0x1C } else { 0x2C });
            frame_data.push(0x01); // packet type = NALU
            frame_data.extend_from_slice(&[0, 0, 0]); // composition time
            for n in &frame_nalus {
                frame_data.extend_from_slice(&(n.len() as u32).to_be_bytes());
                frame_data.extend_from_slice(n);
            }
            let f = MediaFrame::new_video(
                0,
                CodecId::H265,
                ms,
                ms as u64,
                ms as u64,
                Bytes::from(frame_data),
                self.video_key,
            )
            .with_payload_format(PayloadFormat::Flv);
            let _ = self.source.publish_and_cache(f).await;
            self.video_key = false;
            return;
        }

        if !self.config_sent {
            if let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) {
                let cfg = build_avcc_config(&sps, &pps);
                let mut f = MediaFrame::new_video(
                    0,
                    CodecId::H264,
                    ms,
                    ms as u64,
                    ms as u64,
                    Bytes::from(cfg),
                    false,
                )
                .with_payload_format(PayloadFormat::Flv);
                f.config_frame = true;
                let _ = self.source.publish_and_cache(f).await;
                self.config_sent = true;
            }
        }

        let mut frame_nalus = Vec::new();
        for n in &self.pending_nalus {
            let t = n[0] & 0x1F;
            if t == 7 || t == 8 {
                continue;
            }
            frame_nalus.push(n.clone());
        }
        self.pending_nalus.clear();
        if frame_nalus.is_empty() {
            self.video_key = false;
            return;
        }

        let data = build_avcc_frame(&frame_nalus, self.video_key);
        let f = MediaFrame::new_video(
            0,
            CodecId::H264,
            ms,
            ms as u64,
            ms as u64,
            Bytes::from(data),
            self.video_key,
        )
        .with_payload_format(PayloadFormat::Flv);
        let _ = self.source.publish_and_cache(f).await;
        self.video_key = false;
    }

    async fn handle_audio_rtp(&mut self, data: &[u8], ts: u32) {
        if data.len() < 2 {
            return;
        }
        let au_headers_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        let num_aus = au_headers_len / 16;
        let mut pos = 2;
        let mut sizes = Vec::new();
        for _ in 0..num_aus {
            if pos + 2 > data.len() {
                break;
            }
            let au_header = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let size = ((au_header >> 3) & 0x1FFF) as usize;
            sizes.push(size);
            pos += 2;
        }

        if !self.audio_config_sent {
            if let Some(cfg) = self.audio_config.clone() {
                let mut v = vec![0xAF, 0x00];
                v.extend_from_slice(&cfg);
                let mut af = MediaFrame::new_audio(1, CodecId::AAC, 0, 0, 0, Bytes::from(v))
                    .with_payload_format(PayloadFormat::Flv);
                af.config_frame = true;
                let _ = self.source.publish_and_cache(af).await;
                self.audio_config_sent = true;
            }
        }

        // AAC-LC encodes 1024 samples per frame; derive the per-frame duration
        // in milliseconds so each access unit gets a distinct, monotonic
        // timestamp (an RTP packet may carry several AUs).
        let frame_ms = (1024u64 * 1000).div_ceil(self.audio_sample_rate as u64);
        let mut ms = (ts as u64 * 1000 / self.audio_sample_rate as u64) as u32;
        for size in sizes {
            if pos + size > data.len() {
                break;
            }
            let raw = data[pos..pos + size].to_vec();
            pos += size;
            let mut v = Vec::with_capacity(2 + raw.len());
            v.push(0xAF);
            v.push(0x01);
            v.extend_from_slice(&raw);
            let f =
                MediaFrame::new_audio(1, CodecId::AAC, ms, ms as u64, ms as u64, Bytes::from(v))
                    .with_payload_format(PayloadFormat::Flv);
            let _ = self.source.publish_and_cache(f).await;
            ms = ms.saturating_add(frame_ms as u32);
        }
    }
}

// === SDP parsing & AVCC builders ===

pub(crate) struct SdpInfo {
    pub(crate) video_pt: u8,
    pub(crate) audio_pt: u8,
    pub(crate) video_codec: CodecId,
    pub(crate) sps: Option<Vec<u8>>,
    pub(crate) pps: Option<Vec<u8>>,
    pub(crate) vps: Option<Vec<u8>>,
    pub(crate) audio_config: Option<Vec<u8>>,
    pub(crate) audio_sample_rate: u32,
}

pub(crate) fn parse_sdp(body: &[u8]) -> SdpInfo {
    let mut info = SdpInfo {
        video_pt: 96,
        audio_pt: 97,
        video_codec: CodecId::H264,
        sps: None,
        pps: None,
        vps: None,
        audio_config: None,
        audio_sample_rate: 44100,
    };
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 4 {
                if let Ok(pt) = parts[3].parse::<u8>() {
                    match parts[0] {
                        "video" => info.video_pt = pt,
                        "audio" => info.audio_pt = pt,
                        _ => {}
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(pt) = parts[0].parse::<u8>() {
                    let codec = parts[1].to_lowercase();
                    if codec.contains("h265") || codec.contains("hevc") {
                        if pt == info.video_pt {
                            info.video_codec = CodecId::H265;
                        }
                    } else if (codec.contains("mpeg4-generic") || codec.contains("aac"))
                        && pt == info.audio_pt
                    {
                        if let Some(rate) =
                            codec.split('/').nth(1).and_then(|r| r.parse::<u32>().ok())
                        {
                            info.audio_sample_rate = rate;
                        }
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(pt) = parts[0].parse::<u8>() {
                    let params = parts[1];
                    if pt == info.video_pt {
                        if info.video_codec == CodecId::H265 {
                            // HEVC (RFC 7798) advertises parameter sets individually.
                            if let Some(v) = extract_param(params, "sprop-vps") {
                                info.vps = base64_decode(&v);
                            }
                            if let Some(s) = extract_param(params, "sprop-sps") {
                                info.sps = base64_decode(&s);
                            }
                            if let Some(p) = extract_param(params, "sprop-pps") {
                                info.pps = base64_decode(&p);
                            }
                        } else if let Some(sp) = extract_param(params, "sprop-parameter-sets") {
                            let mut iter = sp.split(',');
                            info.sps = iter.next().and_then(base64_decode);
                            info.pps = iter.next().and_then(base64_decode);
                        }
                    } else if pt == info.audio_pt {
                        if let Some(cfg) = extract_param(params, "config") {
                            info.audio_config = hex_decode(&cfg);
                        }
                    }
                }
            }
        }
    }
    info
}

fn extract_param(params: &str, key: &str) -> Option<String> {
    for kv in params.split(';') {
        let kv = kv.trim();
        if let Some(rest) = kv.strip_prefix(&format!("{}=", key)) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    BASE64.decode(s.trim()).ok()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < s.len() {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).ok()?;
        out.push(byte);
        i += 2;
    }
    Some(out)
}

fn build_avcc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x17); // key frame + H264
    out.push(0x00); // AVC decoder config record
    out.extend_from_slice(&[0, 0, 0]);
    let profile = sps.get(1).copied().unwrap_or(0x42);
    let compat = sps.get(2).copied().unwrap_or(0x00);
    let level = sps.get(3).copied().unwrap_or(0x1E);
    out.push(0x01); // configurationVersion
    out.push(profile);
    out.push(compat);
    out.push(level);
    out.push(0xFF); // lengthSizeMinusOne = 3
    out.push(0xE1); // numSPS = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(0x01); // numPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

fn build_avcc_frame(nalus: &[Vec<u8>], key: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(if key { 0x17 } else { 0x27 });
    out.push(0x01); // AVC NALU
    out.extend_from_slice(&[0, 0, 0]);
    for n in nalus {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

/// Builds the body of an FLV HEVC `video tag` configuration record (the part
/// that follows the 5-byte tag header: frame-type/codec byte, packet-type byte
/// and 3-byte composition time). The fixed 21-byte `HEVCDecoderConfigurationRecord`
/// header is followed by the VPS/SPS/PPS arrays (2-byte NALU lengths), matching
/// the layout `extract_hevc_config`/`extract_hevc_nalus` expect downstream.
fn build_hevc_config_record(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x01); // configurationVersion
    if sps.len() >= 8 {
        out.push(sps[2]); // profile_space | tier | profile_idc
        out.extend_from_slice(&sps[3..7]); // profile_compatibility_flags (4)
        out.extend_from_slice(&[0u8; 6]); // general_constraint_indicator_flags (6)
        out.push(sps[7]); // level_idc
    } else {
        out.push(0x20);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&[0u8; 6]);
        out.push(0x5D);
    }
    out.extend_from_slice(&[0, 0]); // min_spatial_segmentation_idc
    out.push(0); // parallelismType
    out.push(1); // chromaFormat (4:2:0)
    out.push(0); // bitDepthLumaMinus8
    out.push(0); // bitDepthChromaMinus8
    out.extend_from_slice(&[0, 0]); // avgFrameRate (2 bytes)
    out.push(0x0F); // numTemporalLayers=1, lengthSizeMinusOne=3, temporalIdNested=1
    out.push(0x03); // numOfArrays = 3

    for (nal_type, nalu) in [(32u8, vps), (33, sps), (34, pps)] {
        out.push(nal_type);
        out.extend_from_slice(&[0, 1]); // numNalus = 1
        out.extend_from_slice(&(nalu.len() as u16).to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

/// Parses the FLV HEVC configuration record carried in a video frame body and
/// returns the raw VPS/SPS/PPS NALUs (without start codes), if present.
type HevcNalus = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

fn extract_hevc_nalus(data: &[u8]) -> HevcNalus {
    if data.len() < 28 {
        return (None, None, None);
    }
    // numOfArrays lives at offset 27: 5-byte FLV header + 22-byte fixed
    // HEVCDecoderConfigurationRecord header (avgFrameRate is 2 bytes).
    let mut pos = 27;
    let num_arrays = data[pos] as usize;
    pos += 1;
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for _ in 0..num_arrays {
        if pos + 3 > data.len() {
            break;
        }
        let nal_type = data[pos] & 0x3F;
        let num_nalus = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
        pos += 3;
        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                break;
            }
            let nalu_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + nalu_len > data.len() {
                break;
            }
            let nalu = data[pos..pos + nalu_len].to_vec();
            pos += nalu_len;
            match nal_type {
                32 => vps = Some(nalu),
                33 => sps = Some(nalu),
                34 => pps = Some(nalu),
                _ => {}
            }
        }
    }
    (vps, sps, pps)
}

/// Reads the cached HEVC configuration frame from a published source and
/// returns base64-encoded (vps, sps, pps) for the SDP `sprop-*` parameters.
async fn extract_hevc_sprop_from_source(
    source: &Arc<MediaSource>,
) -> Option<(String, String, String)> {
    let config_frames = source.get_cached_config_frames().await;
    for f in &config_frames {
        if f.codec == CodecId::H265 {
            if let (Some(v), Some(s), Some(p)) = extract_hevc_nalus(&f.data) {
                return Some((BASE64.encode(v), BASE64.encode(s), BASE64.encode(p)));
            }
        }
    }
    None
}

fn extract_hevc_sprop_from_asset(asset: &VodAsset) -> Option<(String, String, String)> {
    for frame in asset.playback(0).frames {
        if frame.config_frame && frame.codec == CodecId::H265 {
            if let (Some(vps), Some(sps), Some(pps)) = extract_hevc_nalus(&frame.data) {
                return Some((BASE64.encode(vps), BASE64.encode(sps), BASE64.encode(pps)));
            }
        }
    }
    None
}

fn parse_npt_range(request: &RtspRequest) -> (u64, Option<u64>) {
    let Some(value) = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.trim())
    else {
        return (0, None);
    };
    let Some(value) = value.strip_prefix("npt=") else {
        return (0, None);
    };
    let (start, end) = value.split_once('-').unwrap_or((value, ""));
    let to_ms = |value: &str| {
        value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| (value * 1000.0) as u64)
    };
    (to_ms(start).unwrap_or(0), to_ms(end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Builds a minimal FLV HEVC video tag body carrying an
    /// HEVCDecoderConfigurationRecord with one VPS (32), SPS (33) and PPS (34).
    fn make_hevc_config_data() -> Vec<u8> {
        let mut d = vec![
            0x1C, 0x00, 0x00, 0x00, 0x00, // frame type/codec(12) + packet type 0 + ctime
            0x01, // configurationVersion
            0x20, // profile_space/tier/profile_idc
            0x00, 0x00, 0x00, 0x00, // general_profile_compatibility_flags
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // general_constraint_indicator_flags
            0x5D, // general_level_idc
            0x00, 0x00, // min_spatial_segmentation_idc
            0x00, // parallelismType
            0x01, // chromaFormat
            0x00, // bitDepthLumaMinus8
            0x00, // bitDepthChromaMinus8
            0x00, 0x00, // avgFrameRate (2 bytes)
            0x03, // lengthSizeMinusOne = 1, temporalIdNested = 1
            0x03, // numOfArrays = 3
        ];
        d.extend_from_slice(&[32, 0, 1, 0, 2, 0xAA, 0xBB]); // VPS
        d.extend_from_slice(&[33, 0, 1, 0, 2, 0xCC, 0xDD]); // SPS
        d.extend_from_slice(&[34, 0, 1, 0, 2, 0xEE, 0xFF]); // PPS
        d
    }

    #[test]
    fn hevc_config_nalus_parsed() {
        let d = make_hevc_config_data();
        let (vps, sps, pps) = extract_hevc_nalus(&d);
        assert_eq!(vps, Some(vec![0xAA, 0xBB]));
        assert_eq!(sps, Some(vec![0xCC, 0xDD]));
        assert_eq!(pps, Some(vec![0xEE, 0xFF]));
    }

    #[test]
    fn parses_rtsp_npt_range_in_seconds() {
        let request = RtspRequest {
            method: "PLAY".to_string(),
            uri: "rtsp://127.0.0.1/record/a.mp4".to_string(),
            version: "RTSP/1.0".to_string(),
            headers: [("Range".to_string(), "npt=2.500-8.25".to_string())]
                .into_iter()
                .collect(),
            body: Vec::new(),
        };
        assert_eq!(parse_npt_range(&request), (2500, Some(8250)));
    }

    #[test]
    fn hevc_packetize_config_emits_three_nalus() {
        let frame = MediaFrame::new_video(
            0,
            CodecId::H265,
            0,
            0,
            0,
            Bytes::from(make_hevc_config_data()),
            false,
        );
        let mut seq = 1u16;
        let pkts = RtspSession::packetize_hevc(&frame, 0, 0x1234, &mut seq);
        assert_eq!(pkts.len(), 3);
        // Each packet: 12-byte RTP header + NALU payload.
        assert_eq!(&pkts[0][12..], &[0xAA, 0xBB]);
        assert_eq!(&pkts[1][12..], &[0xCC, 0xDD]);
        assert_eq!(&pkts[2][12..], &[0xEE, 0xFF]);
        // Payload type 96, marker bit set on parameter-set packets.
        assert_eq!(pkts[0][1] & 0x7F, 96);
        assert!((pkts[0][1] & 0x80) != 0);
    }

    #[test]
    fn hevc_packetize_single_nalu() {
        // packet type 1 with one 4-byte NALU (0x40 0x01 0x02 0x03).
        let mut d = vec![0x1C, 0x01, 0x00, 0x00, 0x00, 0, 0, 0, 4];
        d.extend_from_slice(&[0x40, 0x01, 0x02, 0x03]);
        let frame = MediaFrame::new_video(0, CodecId::H265, 0, 0, 0, Bytes::from(d), true);
        let mut seq = 1u16;
        let pkts = RtspSession::packetize_hevc(&frame, 0, 0x1234, &mut seq);
        assert_eq!(pkts.len(), 1);
        assert_eq!(&pkts[0][12..], &[0x40, 0x01, 0x02, 0x03]);
        assert!((pkts[0][1] & 0x80) != 0); // marker set for single NALU
    }

    #[test]
    fn parse_sdp_detects_hevc_and_sprops() {
        let vps = BASE64.encode([0x40u8, 0x01, 0x0C]);
        let sps = BASE64.encode([0x42u8, 0x01, 0x01]);
        let pps = BASE64.encode([0x44u8, 0x01, 0x02]);
        let body = format!(
            "v=0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 H265/90000\r\n\
             a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
            vps, sps, pps
        );
        let sdp = parse_sdp(body.as_bytes());
        assert_eq!(sdp.video_codec, CodecId::H265);
        assert_eq!(sdp.vps, Some(vec![0x40, 0x01, 0x0C]));
        assert_eq!(sdp.sps, Some(vec![0x42, 0x01, 0x01]));
        assert_eq!(sdp.pps, Some(vec![0x44, 0x01, 0x02]));
    }

    #[test]
    fn parse_sdp_keeps_h264_default() {
        let body = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
        let sdp = parse_sdp(body.as_bytes());
        assert_eq!(sdp.video_codec, CodecId::H264);
        assert!(sdp.vps.is_none());
    }

    #[test]
    fn hevc_config_record_roundtrip() {
        let vps = vec![0x40u8, 0x01, 0x0C, 0x00];
        let sps = vec![0x42u8, 0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let pps = vec![0x44u8, 0x01, 0x02];
        let record = build_hevc_config_record(&vps, &sps, &pps);
        // Prepend the 5-byte FLV tag header that emit_video_frame adds so we can
        // verify it round-trips through the same extract_hevc_nalus the HLS/RTSP
        // players use (numOfArrays sits at offset 27 = 5-byte header + 22-byte
        // fixed record header).
        let mut data = vec![0x1C, 0x00, 0, 0, 0];
        data.extend_from_slice(&record);
        assert_eq!(data[27], 0x03);
        let (rv, rs, rp) = extract_hevc_nalus(&data);
        assert_eq!(rv, Some(vps.clone()));
        assert_eq!(rs, Some(sps.clone()));
        assert_eq!(rp, Some(pps.clone()));
    }

    #[test]
    fn packetize_annex_b_video_uses_h264_rtp_payload() {
        let frame = MediaFrame::new_video(
            0,
            CodecId::H264,
            40,
            40,
            40,
            Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88]),
            true,
        )
        .with_payload_format(PayloadFormat::AnnexB);
        let mut seq = 1;
        let packets = RtspSession::packetize_video(&frame, 40, 0x1234, &mut seq);
        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][12..], &[0x65, 0x88]);
        assert_eq!(packets[0][1] & 0x7f, 96);
    }

    #[test]
    fn packetize_adts_aac_and_opus_use_codec_specific_layouts() {
        let aac = MediaFrame::new_audio(
            0,
            CodecId::AAC,
            1000,
            1000,
            1000,
            Bytes::from_static(&[0xff, 0xf1, 0x50, 0x80, 0, 0, 0, 0x11, 0x22]),
        )
        .with_payload_format(PayloadFormat::Adts);
        let mut seq = 1;
        let packets = RtspSession::packetize_audio(&aac, 1000, 1, &mut seq, 48_000);
        assert_eq!(packets.len(), 1);
        assert_eq!(&packets[0][16..], &[0x11, 0x22]);
        assert_eq!(
            u32::from_be_bytes(packets[0][4..8].try_into().unwrap()),
            48_000
        );

        let opus = MediaFrame::new_audio(
            0,
            CodecId::Opus,
            20,
            20,
            20,
            Bytes::from_static(&[0xf8, 0xff, 0xfe]),
        )
        .with_payload_format(PayloadFormat::Opus);
        let packets = RtspSession::packetize_audio(&opus, 20, 1, &mut seq, 48_000);
        assert_eq!(&packets[0][12..], opus.data.as_ref());
        assert_eq!(packets[0][1] & 0x7f, 97);
    }

    #[test]
    fn audio_sdp_matches_actual_codec() {
        let opus = generate_audio_sdp(&AudioInfo {
            codec: CodecId::Opus,
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
        });
        assert!(opus.contains("opus/48000/2"));
        assert!(!opus.contains("MPEG4-GENERIC"));

        let pcma = generate_audio_sdp(&AudioInfo {
            codec: CodecId::G711A,
            sample_rate: 8_000,
            channels: 1,
            bits_per_sample: 8,
        });
        assert!(pcma.contains("RTP/AVP 8"));
        assert!(pcma.contains("PCMA/8000/1"));
    }
}
