use super::parser::{RtspParser, RtspRequest, RtspResponse};
use bytes::{Buf, BufMut, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tracing::{debug, error, info, warn};
use zlmediakit_core::media_frame::MediaFrame;
use zlmediakit_core::media_source::{MediaSource, MediaSourceManager};

pub struct RtspSession {
    stream: Option<TcpStream>,
    peer_addr: String,
    client_ip: String,
    source_manager: Arc<MediaSourceManager>,
    source: Option<Arc<MediaSource>>,
    cseq: u32,
    session_id: String,
    app: String,
    stream_name: String,
    is_publisher: bool,
    tcp_interleaved: bool,
    setup_count: u32,
    udp_video: Option<Arc<UdpSocket>>,
    udp_audio: Option<Arc<UdpSocket>>,
    buffer: BytesMut,
}

impl RtspSession {
    pub fn new(
        stream: TcpStream,
        peer_addr: String,
        source_manager: Arc<MediaSourceManager>,
    ) -> Self {
        let session_id = format!("{:08x}", rand::random::<u32>());
        let client_ip = peer_addr.split(':').next().unwrap_or("127.0.0.1").to_string();
        Self {
            stream: Some(stream),
            peer_addr,
            client_ip,
            source_manager,
            source: None,
            cseq: 0,
            session_id,
            app: String::new(),
            stream_name: String::new(),
            is_publisher: false,
            tcp_interleaved: false,
            setup_count: 0,
            udp_video: None,
            udp_audio: None,
            buffer: BytesMut::with_capacity(4096),
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        info!("RTSP session started from {}", self.peer_addr);

        let mut read_buf = [0u8; 4096];
        loop {
            if self.stream.is_none() {
                break;
            }
            let stream = self.stream.as_mut().unwrap();
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
            "PLAY" => self.handle_play().await,
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
        info!("RTSP DESCRIBE: app={}, stream={}", self.app, self.stream_name);

        let source = self
            .source_manager
            .get("__defaultVhost__", &self.app, &self.stream_name);

        info!("RTSP DESCRIBE source found: {}", source.is_some());

        match source {
            Some(source) => {
                self.source = Some(source.clone());

                let sdp = self.generate_sdp();
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

    async fn handle_play(&mut self) -> anyhow::Result<()> {
        self.is_publisher = false;

        let response = RtspResponse::new(200, "OK")
            .with_header("CSeq", &self.cseq.to_string())
            .with_header("Session", &self.session_id);
        self.send_response(&response).await?;

        if let Some(ref source) = self.source {
            let cached = {
                let cache = source.gop_cache.read().await;
                cache.get_latest_gop_frames()
            };

            let mut rx = source.subscribe();

            if self.tcp_interleaved {
                let stream = self.stream.take().unwrap();
                let handle = tokio::spawn(async move {
                    let mut writer = stream;
                    let mut seq: u16 = 1;
                    let ssrc: u32 = rand::random();

                    for frame in &cached {
                        let _ = Self::send_interleaved_frame(&mut writer, frame, &mut seq, ssrc).await;
                    }

                    loop {
                        match rx.recv().await {
                            Ok(frame) => {
                                let _ = Self::send_interleaved_frame(&mut writer, &frame, &mut seq, ssrc).await;
                            }
                            Err(_) => break,
                        }
                    }
                });

                handle.await.ok();
            } else {
                let video_sock = self.udp_video.take().unwrap();
                let audio_sock = self.udp_audio.take();
                tokio::spawn(async move {
                    let mut video_seq: u16 = 1;
                    let mut audio_seq: u16 = 1;
                    let video_ssrc: u32 = rand::random();
                    let audio_ssrc: u32 = rand::random();

                    for frame in &cached {
                        match frame.frame_type {
                            zlmediakit_core::media_frame::FrameType::Video => {
                                let _ = Self::send_udp_frame(&video_sock, frame, &mut video_seq, video_ssrc).await;
                            }
                            zlmediakit_core::media_frame::FrameType::Audio => {
                                if let Some(ref sock) = audio_sock {
                                    let _ = Self::send_udp_frame(sock, frame, &mut audio_seq, audio_ssrc).await;
                                }
                            }
                            _ => {}
                        }
                    }

                    loop {
                        match rx.recv().await {
                            Ok(frame) => {
                                match frame.frame_type {
                                    zlmediakit_core::media_frame::FrameType::Video => {
                                        let _ = Self::send_udp_frame(&video_sock, &frame, &mut video_seq, video_ssrc).await;
                                    }
                                    zlmediakit_core::media_frame::FrameType::Audio => {
                                        if let Some(ref sock) = audio_sock {
                                            let _ = Self::send_udp_frame(sock, &frame, &mut audio_seq, audio_ssrc).await;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }

        Ok(())
    }

    async fn handle_announce(&mut self, request: &RtspRequest) -> anyhow::Result<()> {
        self.is_publisher = true;
        self.parse_uri(&request.uri);

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
        self.send_response(&response).await
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
        let path = uri.trim_start_matches("rtsp://");
        let path = path.splitn(2, '/').nth(1).unwrap_or("");
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        self.app = parts.first().map_or("live", |v| v).to_string();
        self.stream_name = parts.get(1).map_or("stream", |v| v).to_string();
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

    fn generate_sdp(&self) -> String {
        format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 127.0.0.1\r\n\
             s=zlmediakit-rs\r\n\
             c=IN IP4 0.0.0.0\r\n\
             t=0 0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42C01E\r\n\
             a=control:track1\r\n\
             m=audio 0 RTP/AVP 97\r\n\
             a=rtpmap:97 MPEG4-GENERIC/44100/1\r\n\
             a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3\r\n\
             a=control:track2\r\n"
        )
    }

    fn make_rtp_header(
        seq: u16,
        timestamp: u32,
        ssrc: u32,
        pt: u8,
        marker: bool,
    ) -> [u8; 12] {
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

    fn packetize_h264(
        data: &[u8],
        timestamp: u32,
        ssrc: u32,
        seq: &mut u16,
    ) -> Vec<Vec<u8>> {
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
                        let nalu_len =
                            u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
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
                            let nalu_len =
                                u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
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

    fn packetize_audio(
        data: &[u8],
        timestamp: u32,
        ssrc: u32,
        seq: &mut u16,
    ) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        if data.len() < 2 {
            return packets;
        }

        let sound_format = (data[0] >> 4) & 0x0F;
        let rtp_ts = timestamp * 44;
        let pt = if sound_format == 10 { 97 } else { 98 };

        let pos = if sound_format == 10 { 2 } else { 1 };
        let audio_data = &data[pos..];

        if audio_data.is_empty() {
            return packets;
        }

        if sound_format == 10 {
            // AAC: skip config frames (AACPacketType=0)
            if data.len() >= 2 && data[1] == 0 {
                return packets;
            }
            // RFC 3640 format with AU headers
            let au_size = audio_data.len();
            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, pt, true);
            *seq = seq.wrapping_add(1);
            let mut pkt = Vec::with_capacity(12 + 2 + 2 + au_size);
            pkt.extend_from_slice(&hdr);
            pkt.put_u16(16);
            let au_header = ((au_size as u16) << 3) & 0xFFF8;
            pkt.put_u16(au_header);
            pkt.extend_from_slice(audio_data);
            packets.push(pkt);
        } else {
            // MP3 or other: raw RTP payload
            let hdr = Self::make_rtp_header(*seq, rtp_ts, ssrc, pt, true);
            *seq = seq.wrapping_add(1);
            let mut pkt = Vec::with_capacity(12 + audio_data.len());
            pkt.extend_from_slice(&hdr);
            pkt.extend_from_slice(audio_data);
            packets.push(pkt);
        }

        packets
    }

    async fn send_interleaved_rtp(
        stream: &mut tokio::net::TcpStream,
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
    ) -> anyhow::Result<()> {
        let packets = match frame.frame_type {
            zlmediakit_core::media_frame::FrameType::Video => {
                Self::packetize_h264(&frame.data, frame.timestamp, ssrc, seq)
            }
            zlmediakit_core::media_frame::FrameType::Audio => {
                Self::packetize_audio(&frame.data, frame.timestamp, ssrc, seq)
            }
            _ => return Ok(()),
        };

        for pkt in packets {
            let _ = socket.send(&pkt).await;
        }
        Ok(())
    }

    async fn send_interleaved_frame(
        stream: &mut tokio::net::TcpStream,
        frame: &MediaFrame,
        seq: &mut u16,
        ssrc: u32,
    ) -> anyhow::Result<()> {
        let packets = match frame.frame_type {
            zlmediakit_core::media_frame::FrameType::Video => {
                Self::packetize_h264(&frame.data, frame.timestamp, ssrc, seq)
            }
            zlmediakit_core::media_frame::FrameType::Audio => {
                Self::packetize_audio(&frame.data, frame.timestamp, ssrc, seq)
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
        self.source = None;
        self.udp_video = None;
        self.udp_audio = None;
    }
}
