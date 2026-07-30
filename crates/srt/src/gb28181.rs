//! GB28181 RTP/PS push receiver.
//!
//! Listens on a UDP port for RTP packets containing MPEG-PS data,
//! demuxes PS to extract H.264/H.265 NAL units, and publishes
//! them to the MediaSourceManager.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{info, warn};
use zlmediakit_codec::ps::{extract_nalus_from_pes, parse_one_pes};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;

pub struct Gb28181Config {
    pub addr: String,
    pub source_manager: Arc<MediaSourceManager>,
}

/// GB28181 RTP/PS push server. Binds a UDP port and receives RTP/PS
/// streams, publishing extracted video to MediaSourceManager.
pub struct Gb28181Server {
    config: Gb28181Config,
}

impl Gb28181Server {
    pub fn new(config: Gb28181Config) -> Self {
        Self { config }
    }

    /// Runs the server until `stop` is set. To be spawned in a tokio task.
    pub async fn run(&self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        let addr: SocketAddr = self.config.addr.parse()?;
        let socket = UdpSocket::bind(addr).await?;
        info!("GB28181 RTP/PS receiver listening on {}", addr);

        let sm = self.config.source_manager.clone();
        let source = sm.get_or_create("__defaultVhost__", "live", "gb28181");

        let mut buf = vec![0u8; 65536];
        let mut timestamp: u32 = 0;
        let mut seq: u16 = 0;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                socket.recv_from(&mut buf),
            )
            .await
            {
                Ok(Ok((n, _src))) => {
                    if n < 12 {
                        continue; // RTP header minimum
                    }
                    let rtp_payload = &buf[..n];

                    // Parse RTP header
                    let _pt = rtp_payload[1] & 0x7F;
                    let marker = (rtp_payload[1] & 0x80) != 0;
                    let rtp_seq = u16::from_be_bytes([rtp_payload[2], rtp_payload[3]]);
                    let _rtp_ts =
                        u32::from_be_bytes([rtp_payload[4], rtp_payload[5], rtp_payload[6], rtp_payload[7]]);

                    if seq != 0 && rtp_seq != seq.wrapping_add(1) {
                        // Packet loss detected — skip frame boundary detection
                    }
                    seq = rtp_seq;

                    // RTP payload starts after the 12-byte header (no CSRCs assumed)
                    let payload = &rtp_payload[12..];

                    // Parse PS data from RTP payload
                    if let Ok(pes_result) = parse_one_pes(payload) {
                        if let Some((pes_pkt, _consumed)) = pes_result {
                            // Extract NAL units from PES video payload
                            let nalus = extract_nalus_from_pes(&pes_pkt.data);
                            for nalu in &nalus {
                                let codec = detect_codec_from_nalu(nalu);
                                let key_frame = is_idr_nalu(nalu);
                                let frame = MediaFrame::new_video(
                                    0,
                                    codec,
                                    timestamp,
                                    timestamp as u64,
                                    timestamp as u64,
                                    nalu.clone(),
                                    key_frame,
                                );
                                source.publish_and_cache(frame).await;
                            }
                        }
                    }

                    if marker {
                        timestamp += 40; // ~25fps
                    }
                }
                Ok(Err(e)) => {
                    warn!("GB28181 recv error: {}", e);
                }
                Err(_timeout) => {
                    // Timeout — just check stop signal again
                }
            }
        }

        info!("GB28181 RTP/PS receiver stopped");
        Ok(())
    }
}

fn detect_codec_from_nalu(data: &[u8]) -> CodecId {
    if data.len() < 5 {
        return CodecId::H264;
    }
    // Check for Annex-B start code
    let offset = if data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x00 && data[3] == 0x01 {
        4
    } else if data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x01 {
        3
    } else {
        return CodecId::H264;
    };
    if data.len() > offset {
        let nal_type = data[offset] & 0x1F;
        // H.264 NAL types 1-23; H.265 uses a different range
        if nal_type == 0 {
            // HEVC: check next byte
            if data.len() > offset + 1 {
                let hevc_type = ((nal_type as u16) << 1) | ((data[offset + 1] >> 7) as u16);
                if hevc_type <= 40 {
                    return CodecId::H265;
                }
            }
        }
    }
    CodecId::H264
}

fn is_idr_nalu(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }
    let offset = if data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x00 && data[3] == 0x01 {
        4
    } else if data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x01 {
        3
    } else {
        return false;
    };
    if data.len() > offset {
        let nal_type = data[offset] & 0x1F;
        return nal_type == 5 || nal_type == 7;
    }
    false
}
