use super::amf::{AmfDecoder, AmfEncoder, AmfValue};
use super::handshake::RtmpHandshake;
use super::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::event_bus::{Event, EventBus};
use zlmediakit_core::hook::{HookClient, HookResult};
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};
use zlmediakit_core::transport::TransportStream;

pub struct RtmpSession {
    stream: Option<TransportStream>,
    peer_addr: String,
    msg_parser: RtmpMessageParser,
    msg_encoder: RtmpMessageEncoder,
    source_manager: Arc<MediaSourceManager>,
    event_bus: Arc<EventBus>,
    auth: Arc<StreamAuth>,
    hook: Arc<HookClient>,
    source: Option<Arc<MediaSource>>,
    app: String,
    stream_name: String,
    is_publisher: bool,
    stream_id: u32,
    total_read: u64,
    client_win_size: u32,
    last_ack_seq: u64,
}

impl RtmpSession {
    pub fn new(
        stream: TransportStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
        event_bus: Arc<EventBus>,
        auth: Arc<StreamAuth>,
        hook: Arc<HookClient>,
    ) -> Self {
        Self {
            stream: Some(stream),
            peer_addr,
            msg_parser: RtmpMessageParser::new(),
            msg_encoder: RtmpMessageEncoder::new(),
            source_manager,
            event_bus,
            auth,
            hook,
            source: None,
            app: String::new(),
            stream_name: String::new(),
            is_publisher: false,
            stream_id: 0,
            total_read: 0,
            client_win_size: 2500000,
            last_ack_seq: 0,
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        {
            let stream = self.stream.as_mut().unwrap();
            RtmpHandshake::server_handshake(stream).await?;
        }
        info!("RTMP handshake completed from {}", self.peer_addr);

        let mut buf = [0u8; 65536];
        while let Some(stream) = self.stream.as_mut() {
            match stream.read(&mut buf).await {
                Ok(0) => {
                    debug!("RTMP connection closed: {}", self.peer_addr);
                    break;
                }
                Ok(n) => {
                    self.total_read += n as u64;
                    let messages = self.msg_parser.feed(&buf[..n]);
                    for m in &messages {
                        info!(
                            "MSG: type={} ts={} stream_id={} len={}",
                            m.type_id(),
                            m.timestamp(),
                            m.stream_id(),
                            m.payload().len()
                        );
                    }
                    if messages.is_empty()
                        && n > 0
                        && (self.total_read > 5000 || self.msg_parser.has_stuck_data())
                    {
                        let alert = if self.msg_parser.has_stuck_data() {
                            "STUCK"
                        } else {
                            "stall"
                        };
                        let parser_prefix: Vec<String> = self
                            .msg_parser
                            .peek_front(n.min(48))
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        let read_prefix: Vec<String> = buf[..n.min(16)]
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        let b0 = buf[0];
                        warn!(
                            "RTMP {}: n={}, total={}, first=0x{:02x}(fmt={},csid_raw={}), parser_buf={} read=[{}] parser_front=[{}]",
                            alert,
                            n,
                            self.total_read,
                            b0,
                            (b0 >> 6) & 0x03,
                            b0 & 0x3F,
                            self.msg_parser.buffer_len(),
                            read_prefix.join(" "),
                            parser_prefix.join(" "),
                        );
                    }
                    info!(
                        "RTMP read: {} bytes (total={}), {} messages",
                        n,
                        self.total_read,
                        messages.len()
                    );
                    for msg in messages {
                        if let Err(e) = self.handle_message(msg).await {
                            warn!("RTMP message error: {}", e);
                            break;
                        }
                    }
                    self.send_window_ack().await;
                }
                Err(e) => {
                    error!("RTMP read error: {}", e);
                    break;
                }
            }
        }

        self.cleanup().await;
        Ok(())
    }

    async fn send_msg(&mut self, msg: &RtmpMessage) -> anyhow::Result<()> {
        if let Some(ref mut stream) = self.stream {
            let encoded = self.msg_encoder.encode(msg);
            stream.write_all(&encoded).await?;
        }
        Ok(())
    }

    async fn handle_message(&mut self, msg: RtmpMessage) -> anyhow::Result<()> {
        match msg {
            RtmpMessage::SetChunkSize(size) => {
                self.msg_parser.set_chunk_size(size);
                debug!("Set chunk size: {}", size);
            }
            RtmpMessage::WindowAckSize(size) => {
                self.client_win_size = size;
                self.send_msg(&RtmpMessage::SetPeerBandwidth(size, 2))
                    .await?;
                self.send_window_ack().await;
            }
            RtmpMessage::Amf0Command { data, .. } => {
                let values = AmfDecoder::decode(&data)?;
                if let Some(AmfValue::String(cmd)) = values.first() {
                    match cmd.as_str() {
                        "connect" => self.handle_connect(&values).await?,
                        "releaseStream" => {
                            if let Some(AmfValue::String(key)) = values.get(1) {
                                info!("releaseStream key: {}", key);
                                self.parse_stream_key(key);
                                info!(
                                    "After parse_stream_key: app={}, stream={}",
                                    self.app, self.stream_name
                                );
                            }
                        }
                        "FCPublish" => debug!("RTMP FCPublish"),
                        "createStream" => self.handle_create_stream(&values).await?,
                        "publish" => self.handle_publish(&values).await?,
                        "play" => self.handle_play(&values).await?,
                        "deleteStream" => self.handle_delete_stream().await?,
                        "FCUnpublish" => debug!("RTMP FCUnpublish"),
                        "closeStream" => self.handle_close_stream().await?,
                        "getStreamLength" => {
                            self.send_msg(&RtmpMessage::Amf0Command {
                                stream_id: 0,
                                timestamp: 0,
                                data: AmfEncoder::encode(&[
                                    AmfValue::String("_result".to_string()),
                                    values.get(1).cloned().unwrap_or(AmfValue::Number(0.0)),
                                    AmfValue::Null,
                                    AmfValue::Number(0.0),
                                ])
                                .freeze(),
                            })
                            .await?;
                        }
                        _ => debug!("Unknown RTMP command: {}", cmd),
                    }
                }
            }
            RtmpMessage::Amf0Data { data, .. } => {
                let values = AmfDecoder::decode(&data)?;
                if let Some(AmfValue::String(cmd)) = values.first() {
                    match cmd.as_str() {
                        "@setDataFrame" => debug!("Received @setDataFrame"),
                        "onMetaData" => {
                            debug!("Received onMetaData");
                            if self.is_publisher {
                                let mut meta_frame =
                                    MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, data, false);
                                meta_frame.frame_type = FrameType::Metadata;
                                if let Some(ref source) = self.source {
                                    source.publish_and_cache(meta_frame).await;
                                }
                            }
                        }
                        "onBadMP4" => debug!("Received onBadMP4"),
                        _ => debug!("Unknown RTMP data: {}", cmd),
                    }
                }
            }
            RtmpMessage::UserControl(event_type, data) => {
                debug!("UserControl event: {}", event_type);
                if event_type == 6 && data.len() >= 4 {
                    let seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    self.send_msg(&RtmpMessage::Ack(seq)).await?;
                }
            }
            RtmpMessage::Ack(_) => {}
            RtmpMessage::SetPeerBandwidth(_, _) => {}
            RtmpMessage::Video {
                stream_id,
                timestamp,
                data,
            } => {
                if self.is_publisher {
                    info!(
                        "VIDEO: ts={} stream_id={} len={} first_bytes={:02x?}",
                        timestamp,
                        stream_id,
                        data.len(),
                        &data[..data.len().min(8)]
                    );
                    if let (Some(ref source), Some(data)) =
                        (&self.source, normalize_enhanced_video(&data))
                    {
                        let (codec, key_frame, is_config) = parse_video_rtmp_packet(&data);
                        if is_config && source.info.read().await.tracks.is_empty() {
                            source.info.write().await.tracks.push(
                                zlmediakit_core::media_frame::TrackInfo::Video(
                                    zlmediakit_core::media_frame::VideoInfo {
                                        codec,
                                        width: 640,
                                        height: 480,
                                        fps: 25.0,
                                        key_frame: false,
                                    },
                                ),
                            );
                        }
                        let mut frame = MediaFrame::new_video(
                            0,
                            codec,
                            timestamp,
                            timestamp as u64,
                            timestamp as u64,
                            data,
                            key_frame,
                        );
                        frame.config_frame = is_config;
                        source.publish_and_cache(frame).await;
                    }
                }
            }
            RtmpMessage::Audio {
                stream_id: _,
                timestamp,
                data,
            } => {
                if self.is_publisher {
                    if let Some(ref source) = self.source {
                        let codec = parse_audio_rtmp_packet(&data);
                        let is_config =
                            data.len() >= 2 && (data[0] >> 4) & 0x0F == 10 && data[1] == 0x00;
                        if is_config {
                            let has_audio = source.info.read().await.tracks.iter().any(|t| {
                                matches!(t, zlmediakit_core::media_frame::TrackInfo::Audio(_))
                            });
                            if !has_audio {
                                source.info.write().await.tracks.push(
                                    zlmediakit_core::media_frame::TrackInfo::Audio(
                                        zlmediakit_core::media_frame::AudioInfo {
                                            codec,
                                            sample_rate: 44100,
                                            channels: 2,
                                            bits_per_sample: 16,
                                        },
                                    ),
                                );
                            }
                        }
                        let mut frame = MediaFrame::new_audio(
                            1,
                            codec,
                            timestamp,
                            timestamp as u64,
                            timestamp as u64,
                            data,
                        );
                        frame.config_frame = is_config;
                        source.publish_and_cache(frame).await;
                    }
                }
            }
            RtmpMessage::Abort(stream_id) => {
                debug!("Abort stream_id={}", stream_id);
            }
        }
        Ok(())
    }

    async fn send_window_ack(&mut self) {
        let unacked = self.total_read.saturating_sub(self.last_ack_seq);
        if unacked >= self.client_win_size as u64 {
            if let Some(ref mut stream) = self.stream {
                let ack = RtmpMessage::Ack(self.total_read as u32);
                let encoded = self.msg_encoder.encode(&ack);
                if let Err(e) = stream.write_all(&encoded).await {
                    warn!("Failed to send window ack: {}", e);
                }
                self.last_ack_seq = self.total_read;
            }
        }
    }

    fn parse_stream_key(&mut self, key: &str) {
        self.stream_name = key.to_string();
    }

    async fn handle_connect(&mut self, values: &[AmfValue]) -> anyhow::Result<()> {
        debug!("RTMP connect, values count={}", values.len());

        // Connect: ["connect", transactionId, {tcUrl, ...}]
        let props_obj = values.get(2).or_else(|| values.get(1));
        if let Some(AmfValue::Object(props)) = props_obj {
            for (key, val) in props {
                if key == "tcUrl" {
                    if let AmfValue::String(tc_url) = val {
                        info!("tcUrl: {}", tc_url);
                        self.parse_tc_url(tc_url);
                        info!("Parsed app={}, stream={}", self.app, self.stream_name);
                    }
                }
            }
        } else {
            info!(
                "connect: no object found at index 2 or 1, values[1]={:?}",
                values.get(1)
            );
        }

        self.send_msg(&RtmpMessage::WindowAckSize(2500000)).await?;
        self.send_msg(&RtmpMessage::SetPeerBandwidth(2500000, 2))
            .await?;
        // Negotiate a larger chunk size with the client. This tells the client
        // to *read* our messages at 4096-byte chunks, so the encoder must emit
        // at the same size. The server's own *parser* stays at the client's
        // send chunk size (128) — never change it here.
        self.send_msg(&RtmpMessage::SetChunkSize(4096)).await?;
        self.msg_encoder.set_chunk_size(4096);

        let server_props = vec![
            (
                "fmsVer".to_string(),
                AmfValue::String("FMS/3,0,1,123".to_string()),
            ),
            ("capabilities".to_string(), AmfValue::Number(31.0)),
        ];

        let info_props = vec![
            ("level".to_string(), AmfValue::String("status".to_string())),
            (
                "code".to_string(),
                AmfValue::String("NetConnection.Connect.Success".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String("Connection succeeded.".to_string()),
            ),
            ("objectEncoding".to_string(), AmfValue::Number(0.0)),
            (
                "data".to_string(),
                AmfValue::Object(vec![
                    (
                        "version".to_string(),
                        AmfValue::String("FMS/3,0,1,123".to_string()),
                    ),
                    ("capabilities".to_string(), AmfValue::Number(31.0)),
                ]),
            ),
        ];

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: 0,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("_result".to_string()),
                AmfValue::Number(1.0),
                AmfValue::Object(server_props),
                AmfValue::Object(info_props),
            ])
            .freeze(),
        })
        .await?;

        let info_props = vec![
            ("level".to_string(), AmfValue::String("status".to_string())),
            (
                "code".to_string(),
                AmfValue::String("NetConnection.Connect.Success".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String("Connection succeeded.".to_string()),
            ),
            ("objectEncoding".to_string(), AmfValue::Number(0.0)),
        ];

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: 0,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("onStatus".to_string()),
                AmfValue::Number(0.0),
                AmfValue::Null,
                AmfValue::Object(info_props),
            ])
            .freeze(),
        })
        .await?;

        Ok(())
    }

    fn parse_tc_url(&mut self, tc_url: &str) {
        let url = tc_url
            .trim_start_matches("rtmp://")
            .trim_start_matches("rtmps://");
        let parts: Vec<&str> = url.splitn(2, '/').collect();
        if parts.len() >= 2 {
            let app_parts: Vec<&str> = parts[1].splitn(2, '/').collect();
            self.app = app_parts.first().map_or("live", |v| v).to_string();
            self.stream_name = app_parts.get(1).map_or("", |v| v).to_string();
        }
    }

    async fn handle_create_stream(&mut self, values: &[AmfValue]) -> anyhow::Result<()> {
        debug!("RTMP createStream");
        let txn_id = values
            .get(1)
            .and_then(|v| match v {
                AmfValue::Number(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(1.0);

        self.stream_id = 1;

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: 0,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("_result".to_string()),
                AmfValue::Number(txn_id),
                AmfValue::Null,
                AmfValue::Number(self.stream_id as f64),
            ])
            .freeze(),
        })
        .await?;
        Ok(())
    }

    async fn handle_publish(&mut self, values: &[AmfValue]) -> anyhow::Result<()> {
        debug!("RTMP publish");
        self.is_publisher = true;

        // Publish command: ["publish", transactionId, null, streamName, publishType]
        if let Some(AmfValue::String(name)) = values.get(3) {
            if !name.is_empty() {
                self.stream_name = name.clone();
            }
        }
        if let Some(AmfValue::String(type_str)) = values.get(4) {
            debug!("Publish type: {}", type_str);
        }

        if self.stream_name.is_empty() {
            warn!("No stream name provided for publish");
        }

        let (stream_base, sign) = Self::split_stream_sign(&self.stream_name);
        self.stream_name = stream_base;

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_publish("__defaultVhost__", &self.app, &self.stream_name, &sign)
            .await
        {
            warn!(
                "RTMP publish rejected (hook): {}/{} - {}",
                self.app, self.stream_name, msg
            );
            return self.send_publish_not_allowed(&msg).await;
        }

        if !self.auth.check(
            "__defaultVhost__",
            &self.app,
            &self.stream_name,
            "pub",
            &sign,
        ) {
            warn!(
                "RTMP publish rejected (auth): {}/{}",
                self.app, self.stream_name
            );
            return self.send_publish_not_allowed("auth failed").await;
        }

        let source =
            self.source_manager
                .get_or_create("__defaultVhost__", &self.app, &self.stream_name);

        source.info.write().await.tracks.clear();

        self.source = Some(source.clone());

        // Notify the recorder supervisor (and any future hooks) that a stream
        // has started publishing.
        self.event_bus.publish(Event::StreamPublish {
            source_id: source.id.clone(),
            vhost: "__defaultVhost__".to_string(),
            app: self.app.clone(),
            stream: self.stream_name.clone(),
        });

        let status_obj = AmfValue::Object(vec![
            ("level".to_string(), AmfValue::String("status".to_string())),
            (
                "code".to_string(),
                AmfValue::String("NetStream.Publish.Start".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String(format!("Publishing on {}/{}", self.app, self.stream_name)),
            ),
        ]);

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: self.stream_id,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("onStatus".to_string()),
                AmfValue::Number(0.0),
                AmfValue::Null,
                status_obj,
            ])
            .freeze(),
        })
        .await?;

        info!("RTMP publish started: {}/{}", self.app, self.stream_name);
        Ok(())
    }

    async fn handle_play(&mut self, values: &[AmfValue]) -> anyhow::Result<()> {
        debug!("RTMP play");
        self.is_publisher = false;

        // Play command: ["play", transactionId, null, streamName, start, duration, reset]
        if let Some(AmfValue::String(name)) = values.get(3) {
            if !name.is_empty() {
                self.stream_name = name.clone();
            }
        } else if let Some(AmfValue::String(name)) = values.get(1) {
            // Some clients send just ["play", streamName]
            if !name.is_empty() {
                self.stream_name = name.clone();
            }
        }

        // Keep self.stream_id from createStream (already set)

        let (stream_base, sign) = Self::split_stream_sign(&self.stream_name);
        self.stream_name = stream_base;

        // External hook callback (before built-in auth)
        if let HookResult::Deny(msg) = self
            .hook
            .on_play("__defaultVhost__", &self.app, &self.stream_name, &sign)
            .await
        {
            warn!(
                "RTMP play rejected (hook): {}/{} - {}",
                self.app, self.stream_name, msg
            );
            return self.send_play_not_allowed(&msg).await;
        }

        if !self.auth.check(
            "__defaultVhost__",
            &self.app,
            &self.stream_name,
            "play",
            &sign,
        ) {
            warn!(
                "RTMP play rejected (auth): {}/{}",
                self.app, self.stream_name
            );
            return self.send_play_not_allowed("auth failed").await;
        }

        self.send_msg(&RtmpMessage::UserControl(
            0,
            self.stream_id.to_be_bytes().to_vec(),
        ))
        .await?;

        let source = self
            .source_manager
            .get("__defaultVhost__", &self.app, &self.stream_name);

        match source {
            Some(source) => {
                self.source = Some(source.clone());

                let reset_status = AmfValue::Object(vec![
                    ("level".to_string(), AmfValue::String("status".to_string())),
                    (
                        "code".to_string(),
                        AmfValue::String("NetStream.Play.Reset".to_string()),
                    ),
                    (
                        "description".to_string(),
                        AmfValue::String(format!(
                            "Playing reset of {}/{}",
                            self.app, self.stream_name
                        )),
                    ),
                ]);
                self.send_msg(&RtmpMessage::Amf0Command {
                    stream_id: 0,
                    timestamp: 0,
                    data: AmfEncoder::encode(&[
                        AmfValue::String("onStatus".to_string()),
                        AmfValue::Number(0.0),
                        AmfValue::Null,
                        reset_status,
                    ])
                    .freeze(),
                })
                .await?;

                let start_status = AmfValue::Object(vec![
                    ("level".to_string(), AmfValue::String("status".to_string())),
                    (
                        "code".to_string(),
                        AmfValue::String("NetStream.Play.Start".to_string()),
                    ),
                    (
                        "description".to_string(),
                        AmfValue::String(format!("Playing {}/{}", self.app, self.stream_name)),
                    ),
                ]);
                self.send_msg(&RtmpMessage::Amf0Command {
                    stream_id: 0,
                    timestamp: 0,
                    data: AmfEncoder::encode(&[
                        AmfValue::String("onStatus".to_string()),
                        AmfValue::Number(0.0),
                        AmfValue::Null,
                        start_status,
                    ])
                    .freeze(),
                })
                .await?;

                {
                    let cached = {
                        let cache = source.gop_cache.read().await;
                        cache.get_latest_gop_frames()
                    };
                    let encoder = self.msg_encoder.clone();
                    let stream_id = self.stream_id;
                    let stream = match self.stream.take() {
                        Some(s) => s,
                        None => return Ok(()),
                    };

                    tokio::spawn(async move {
                        let mut writer = stream;

                        for frame in cached {
                            let msg = match frame.frame_type {
                                FrameType::Video => RtmpMessage::Video {
                                    stream_id,
                                    timestamp: frame.timestamp,
                                    data: frame.data,
                                },
                                FrameType::Audio => RtmpMessage::Audio {
                                    stream_id,
                                    timestamp: frame.timestamp,
                                    data: frame.data,
                                },
                                FrameType::Metadata => RtmpMessage::Amf0Data {
                                    stream_id,
                                    timestamp: frame.timestamp,
                                    data: frame.data,
                                },
                            };
                            let encoded = encoder.encode(&msg);
                            if writer.write_all(&encoded).await.is_err() {
                                return;
                            }
                        }

                        let mut rx = source.subscribe();
                        loop {
                            match rx.recv().await {
                                Ok(frame) => {
                                    if zlmediakit_core::gop_cache::is_config_frame(&frame) {
                                        continue;
                                    }
                                    let msg = match frame.frame_type {
                                        FrameType::Metadata => RtmpMessage::Amf0Data {
                                            stream_id,
                                            timestamp: frame.timestamp,
                                            data: frame.data,
                                        },
                                        FrameType::Video => RtmpMessage::Video {
                                            stream_id,
                                            timestamp: frame.timestamp,
                                            data: frame.data,
                                        },
                                        _ => RtmpMessage::Audio {
                                            stream_id,
                                            timestamp: frame.timestamp,
                                            data: frame.data,
                                        },
                                    };
                                    if writer.write_all(&encoder.encode(&msg)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(_) => break,
                            }
                        }
                    });
                }

                info!("RTMP play started: {}/{}", self.app, self.stream_name);
            }
            None => {
                warn!("Stream not found: {}/{}", self.app, self.stream_name);

                // Fire stream-not-found hook (best-effort, fire-and-forget)
                let hook = self.hook.clone();
                let app = self.app.clone();
                let stream = self.stream_name.clone();
                tokio::spawn(async move {
                    hook.on_stream_not_found("__defaultVhost__", &app, &stream)
                        .await;
                });

                let on_status = AmfValue::Object(vec![
                    ("level".to_string(), AmfValue::String("error".to_string())),
                    (
                        "code".to_string(),
                        AmfValue::String("NetStream.Play.StreamNotFound".to_string()),
                    ),
                    (
                        "description".to_string(),
                        AmfValue::String("Stream not found.".to_string()),
                    ),
                ]);
                self.send_msg(&RtmpMessage::Amf0Command {
                    stream_id: 0,
                    timestamp: 0,
                    data: AmfEncoder::encode(&[
                        AmfValue::String("onStatus".to_string()),
                        AmfValue::Number(0.0),
                        AmfValue::Null,
                        on_status,
                    ])
                    .freeze(),
                })
                .await?;
            }
        }

        Ok(())
    }

    async fn handle_delete_stream(&mut self) -> anyhow::Result<()> {
        debug!("RTMP deleteStream");
        self.cleanup().await;
        Ok(())
    }

    async fn handle_close_stream(&mut self) -> anyhow::Result<()> {
        debug!("RTMP closeStream");
        self.cleanup().await;
        Ok(())
    }

    async fn cleanup(&mut self) {
        info!("RTMP session closing: {}/{}", self.app, self.stream_name);
        if self.is_publisher {
            self.event_bus.publish(Event::StreamUnPublish {
                source_id: format!("__defaultVhost__/{}/{}", self.app, self.stream_name),
                vhost: "__defaultVhost__".to_string(),
                app: self.app.clone(),
                stream: self.stream_name.clone(),
            });
        }
        self.source = None;
    }

    /// Sends `NetStream.Publish.NotAllowed` with a custom description.
    async fn send_publish_not_allowed(&mut self, reason: &str) -> anyhow::Result<()> {
        let status_obj = AmfValue::Object(vec![
            ("level".to_string(), AmfValue::String("error".to_string())),
            (
                "code".to_string(),
                AmfValue::String("NetStream.Publish.NotAllowed".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String(format!("{}", reason)),
            ),
        ]);
        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: self.stream_id,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("onStatus".to_string()),
                AmfValue::Number(0.0),
                AmfValue::Null,
                status_obj,
            ])
            .freeze(),
        })
        .await
    }

    /// Sends `NetStream.Play.NotAllowed` with a custom description.
    async fn send_play_not_allowed(&mut self, reason: &str) -> anyhow::Result<()> {
        let status_obj = AmfValue::Object(vec![
            ("level".to_string(), AmfValue::String("error".to_string())),
            (
                "code".to_string(),
                AmfValue::String("NetStream.Play.NotAllowed".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String(format!("{}", reason)),
            ),
        ]);
        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: self.stream_id,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("onStatus".to_string()),
                AmfValue::Number(0.0),
                AmfValue::Null,
                status_obj,
            ])
            .freeze(),
        })
        .await
    }

    /// Splits a stream name that may carry a `?sign=...` query into the bare
    /// stream name and the extracted signature. Used for token-based auth.
    fn split_stream_sign(name: &str) -> (String, String) {
        match name.split_once('?') {
            Some((base, query)) => {
                let sign = query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(k, _)| *k == "sign")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                (base.to_string(), sign)
            }
            None => (name.to_string(), String::new()),
        }
    }
}

/// Normalizes an enhanced-RTMP (E-RTMP, `IsExHeader` bit set) HEVC video
/// packet into the legacy `codecid=12` FLV layout used by the rest of the
/// pipeline: `[frame|12, packet_type, ct0, ct1, ct2, payload...]`.
///
/// - Non-enhanced packets are passed through unchanged.
/// - `SequenceStart` (0) maps to packet_type 0 (HEVCDecoderConfigurationRecord).
/// - `CodedFrames` (1) keeps its 3-byte composition time; `CodedFramesX` (3)
///   uses a zero composition time.
/// - Packets with no media payload (SequenceEnd, Metadata, ...) or with an
///   unsupported FourCC return `None` and are dropped.
pub(crate) fn normalize_enhanced_video(data: &bytes::Bytes) -> Option<bytes::Bytes> {
    if data.len() < 5 || data[0] & 0x80 == 0 {
        return Some(data.clone());
    }
    let frame_type = (data[0] >> 4) & 0x07;
    let packet_type = data[0] & 0x0F;
    let fourcc = &data[1..5];
    if fourcc != b"hvc1" && fourcc != b"hev1" {
        warn!(
            "Unsupported enhanced-RTMP FourCC: {:?}",
            String::from_utf8_lossy(fourcc)
        );
        return None;
    }
    let first = (frame_type << 4) | 12;
    let mut out = Vec::with_capacity(data.len());
    match packet_type {
        0 => {
            // SequenceStart: payload is the HEVCDecoderConfigurationRecord.
            out.push(first);
            out.push(0);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(&data[5..]);
        }
        1 => {
            // CodedFrames: 3-byte composition time precedes the NALUs.
            if data.len() < 8 {
                return None;
            }
            out.push(first);
            out.push(1);
            out.extend_from_slice(&data[5..8]);
            out.extend_from_slice(&data[8..]);
        }
        3 => {
            // CodedFramesX: no composition time (implicitly zero).
            out.push(first);
            out.push(1);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(&data[5..]);
        }
        _ => return None,
    }
    Some(bytes::Bytes::from(out))
}

pub fn parse_video_rtmp_packet(data: &[u8]) -> (CodecId, bool, bool) {
    if data.len() < 2 {
        return (CodecId::H264, false, false);
    }

    let first_byte = data[0];
    let frame_type = (first_byte >> 4) & 0x0F;
    let codec_id = first_byte & 0x0F;
    let is_keyframe = frame_type == 1;

    // Enhanced RTMP (ExHeader bit set)
    if data[0] & 0x80 != 0 && data.len() >= 5 {
        let fourcc = &data[1..5];
        let packet_type = data[0] & 0x0F;
        let is_config = packet_type == 0;
        let codec = parse_enhanced_video_codec(fourcc);
        return (codec, is_keyframe && !is_config, is_config);
    }

    let is_config = (codec_id == 7 || codec_id == 12) && (data[1] & 0x0F) == 0;

    if codec_id == 7 {
        (CodecId::H264, is_keyframe && !is_config, is_config)
    } else if codec_id == 12 {
        (CodecId::H265, is_keyframe && !is_config, is_config)
    } else {
        (CodecId::H264, is_keyframe, false)
    }
}

pub fn parse_audio_rtmp_packet(data: &[u8]) -> CodecId {
    if data.is_empty() {
        return CodecId::AAC;
    }
    let sound_format = (data[0] >> 4) & 0x0F;
    match sound_format {
        10 => CodecId::AAC,
        2 => CodecId::Mp3,
        7 => CodecId::G711A,
        8 => CodecId::G711U,
        13 => CodecId::Opus,
        14 => CodecId::L16,   // PCM Little-endian 16-bit
        _ => CodecId::AAC,
    }
}

/// Maps enhanced RTMP/FLV codec IDs to our CodecId enum.
/// Enhanced FLV uses FourCC codes: av01=AV1, vp09=VP9, vp08=VP8.
pub fn parse_enhanced_video_codec(fourcc: &[u8]) -> CodecId {
    match fourcc {
        b"av01" => CodecId::AV1,
        b"vp09" => CodecId::VP9,
        b"vp08" => CodecId::VP8,
        b"hvc1" | b"hev1" => CodecId::H265,
        b"avc1" => CodecId::H264,
        _ => CodecId::Unknown(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn enhanced_hevc_sequence_start_maps_to_legacy_config() {
        // frame_type=1 (key), packet_type=0, FourCC hvc1, fake record bytes.
        let mut d = vec![0x80 | (1 << 4)];
        d.extend_from_slice(b"hvc1");
        d.extend_from_slice(&[0x01, 0x02, 0x03]);
        let out = normalize_enhanced_video(&Bytes::from(d)).unwrap();
        assert_eq!(&out[..5], &[0x1C, 0x00, 0, 0, 0]);
        assert_eq!(&out[5..], &[0x01, 0x02, 0x03]);
        let (codec, key, config) = parse_video_rtmp_packet(&out);
        assert_eq!(codec, CodecId::H265);
        assert!(config);
        assert!(!key);
    }

    #[test]
    fn enhanced_hevc_coded_frames_keeps_composition_time() {
        // frame_type=2 (inter), packet_type=1, ct=0x000102, one payload byte.
        let mut d = vec![0x80 | (2 << 4) | 1];
        d.extend_from_slice(b"hvc1");
        d.extend_from_slice(&[0x00, 0x01, 0x02, 0xAA]);
        let out = normalize_enhanced_video(&Bytes::from(d)).unwrap();
        assert_eq!(&out[..5], &[0x2C, 0x01, 0x00, 0x01, 0x02]);
        assert_eq!(&out[5..], &[0xAA]);
        let (codec, key, config) = parse_video_rtmp_packet(&out);
        assert_eq!(codec, CodecId::H265);
        assert!(!config);
        assert!(!key);
    }

    #[test]
    fn enhanced_hevc_coded_frames_x_zero_ct() {
        let mut d = vec![0x80 | (1 << 4) | 3];
        d.extend_from_slice(b"hev1");
        d.extend_from_slice(&[0xBB]);
        let out = normalize_enhanced_video(&Bytes::from(d)).unwrap();
        assert_eq!(&out[..5], &[0x1C, 0x01, 0, 0, 0]);
        assert_eq!(&out[5..], &[0xBB]);
    }

    #[test]
    fn legacy_packets_pass_through_and_metadata_dropped() {
        let legacy = Bytes::from(vec![0x17, 0x00, 0, 0, 0, 0x01]);
        assert_eq!(normalize_enhanced_video(&legacy).unwrap(), legacy);

        // packet_type=4 (Metadata) carries no media and must be dropped.
        let mut meta = vec![0x80 | (1 << 4) | 4];
        meta.extend_from_slice(b"hvc1");
        assert!(normalize_enhanced_video(&Bytes::from(meta)).is_none());

        // Unsupported FourCC (av01) must be dropped.
        let mut av1 = vec![0x80 | (1 << 4)];
        av1.extend_from_slice(b"av01");
        av1.push(0x00);
        assert!(normalize_enhanced_video(&Bytes::from(av1)).is_none());
    }
}
