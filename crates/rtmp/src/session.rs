use super::amf::{AmfDecoder, AmfEncoder, AmfValue};
use super::handshake::RtmpHandshake;
use super::message::{RtmpMessage, RtmpMessageEncoder, RtmpMessageParser};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};

pub struct RtmpSession {
    stream: Option<TcpStream>,
    peer_addr: String,
    msg_parser: RtmpMessageParser,
    msg_encoder: RtmpMessageEncoder,
    source_manager: Arc<MediaSourceManager>,
    source: Option<Arc<MediaSource>>,
    app: String,
    stream_name: String,
    is_publisher: bool,
    stream_id: u32,
}

impl RtmpSession {
    pub fn new(
        stream: TcpStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
    ) -> Self {
        Self {
            stream: Some(stream),
            peer_addr,
            msg_parser: RtmpMessageParser::new(),
            msg_encoder: RtmpMessageEncoder::new(),
            source_manager,
            source: None,
            app: String::new(),
            stream_name: String::new(),
            is_publisher: false,
            stream_id: 0,
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        {
            let stream = self.stream.as_mut().unwrap();
            RtmpHandshake::server_handshake(stream).await?;
        }
        info!("RTMP handshake completed from {}", self.peer_addr);

        let mut buf = [0u8; 65536];
        loop {
            let stream = match self.stream.as_mut() {
                Some(s) => s,
                None => break,
            };
            match stream.read(&mut buf).await {
                Ok(0) => {
                    debug!("RTMP connection closed: {}", self.peer_addr);
                    break;
                }
                Ok(n) => {
                    let messages = self.msg_parser.feed(&buf[..n]);
                    for msg in messages {
                        if let Err(e) = self.handle_message(msg).await {
                            warn!("RTMP message error: {}", e);
                            break;
                        }
                    }
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
                self.send_msg(&RtmpMessage::WindowAckSize(size)).await?;
                self.send_msg(&RtmpMessage::SetPeerBandwidth(size, 2))
                    .await?;
                self.send_msg(&RtmpMessage::SetChunkSize(4096))
                    .await?;
                self.msg_parser.set_chunk_size(4096);
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
                                info!("After parse_stream_key: app={}, stream={}", self.app, self.stream_name);
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
                if event_type == 6 {
                    if data.len() >= 4 {
                        let seq =
                            u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                        self.send_msg(&RtmpMessage::Ack(seq)).await?;
                    }
                }
            }
            RtmpMessage::Ack(_) => {}
            RtmpMessage::SetPeerBandwidth(_, _) => {}
            RtmpMessage::Video {
                stream_id: _,
                timestamp,
                data,
            } => {
                if self.is_publisher {
                    if let Some(ref source) = self.source {
                        if data.len() >= 2 {
                            debug!("RTMP video first 4 bytes: {:02x?}, is_config={}", &data[..std::cmp::min(4, data.len())], (data[0] & 0x0F) == 7 && (data[1] & 0x0F) == 0);
                        }
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
                                    }
                                )
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
                        let frame = MediaFrame::new_audio(
                            1,
                            codec,
                            timestamp,
                            timestamp as u64,
                            timestamp as u64,
                            data,
                        );
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
            info!("connect: no object found at index 2 or 1, values[1]={:?}", values.get(1));
        }

        self.send_msg(&RtmpMessage::WindowAckSize(2500000))
            .await?;
        self.send_msg(&RtmpMessage::SetPeerBandwidth(2500000, 2))
            .await?;
        self.send_msg(&RtmpMessage::SetChunkSize(4096))
            .await?;
        self.msg_parser.set_chunk_size(4096);
        self.msg_encoder.set_chunk_size(4096);

        let server_props = vec![
            ("fmsVer".to_string(), AmfValue::String("FMS/3,0,1,123".to_string())),
            ("capabilities".to_string(), AmfValue::Number(31.0)),
        ];

        let info_props = vec![
            ("level".to_string(), AmfValue::String("status".to_string())),
            ("code".to_string(), AmfValue::String("NetConnection.Connect.Success".to_string())),
            ("description".to_string(), AmfValue::String("Connection succeeded.".to_string())),
            ("objectEncoding".to_string(), AmfValue::Number(0.0)),
            ("data".to_string(), AmfValue::Object(vec![
                ("version".to_string(), AmfValue::String("FMS/3,0,1,123".to_string())),
                ("capabilities".to_string(), AmfValue::Number(31.0)),
            ])),
        ];

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: 0,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("_result".to_string()),
                AmfValue::Number(1.0),
                AmfValue::Object(server_props),
                AmfValue::Object(info_props),
            ]).freeze(),
        })
        .await?;

        let info_props = vec![
            (
                "level".to_string(),
                AmfValue::String("status".to_string()),
            ),
            (
                "code".to_string(),
                AmfValue::String("NetConnection.Connect.Success".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::String("Connection succeeded.".to_string()),
            ),
            (
                "objectEncoding".to_string(),
                AmfValue::Number(0.0),
            ),
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

        let source = self.source_manager.get_or_create(
            "__defaultVhost__",
            &self.app,
            &self.stream_name,
        );

        source.info.write().await.tracks.clear();

        self.source = Some(source.clone());

        let status_obj = AmfValue::Object(vec![
            ("level".to_string(), AmfValue::String("status".to_string())),
            ("code".to_string(), AmfValue::String("NetStream.Publish.Start".to_string())),
            ("description".to_string(), AmfValue::String(format!(
                "Publishing on {}/{}", self.app, self.stream_name
            ))),
        ]);

        self.send_msg(&RtmpMessage::Amf0Command {
            stream_id: self.stream_id,
            timestamp: 0,
            data: AmfEncoder::encode(&[
                AmfValue::String("onStatus".to_string()),
                AmfValue::Number(0.0),
                AmfValue::Null,
                status_obj,
            ]).freeze(),
        }).await?;

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

        self.send_msg(&RtmpMessage::UserControl(0, self.stream_id.to_be_bytes().to_vec()))
            .await?;

        let source = self
            .source_manager
            .get("__defaultVhost__", &self.app, &self.stream_name);

        match source {
            Some(source) => {
                self.source = Some(source.clone());

                let reset_status = AmfValue::Object(vec![
                    (
                        "level".to_string(),
                        AmfValue::String("status".to_string()),
                    ),
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
                    (
                        "level".to_string(),
                        AmfValue::String("status".to_string()),
                    ),
                    (
                        "code".to_string(),
                        AmfValue::String("NetStream.Play.Start".to_string()),
                    ),
                    (
                        "description".to_string(),
                        AmfValue::String(format!(
                            "Playing {}/{}",
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
                    let stream = self.stream.take().unwrap();

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
                let on_status = AmfValue::Object(vec![
                    (
                        "level".to_string(),
                        AmfValue::String("error".to_string()),
                    ),
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
        self.source = None;
    }
}

fn parse_video_rtmp_packet(data: &[u8]) -> (CodecId, bool, bool) {
    if data.len() < 2 {
        return (CodecId::H264, false, false);
    }

    let first_byte = data[0];
    let frame_type = (first_byte >> 4) & 0x0F;
    let codec_id = first_byte & 0x0F;
    let is_keyframe = frame_type == 1;

    let is_config = (codec_id == 7 || codec_id == 12) && (data[1] & 0x0F) == 0;

    if codec_id == 7 {
        (CodecId::H264, is_keyframe && !is_config, is_config)
    } else if codec_id == 12 {
        (CodecId::H265, is_keyframe && !is_config, is_config)
    } else if codec_id == 13 {
        (CodecId::H264, is_keyframe, false)
    } else {
        (CodecId::H264, is_keyframe, false)
    }
}

fn parse_audio_rtmp_packet(data: &[u8]) -> CodecId {
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
        _ => CodecId::AAC,
    }
}
