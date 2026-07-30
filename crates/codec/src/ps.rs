//! MPEG-PS (Program Stream) parser for GB28181 RTP/PS streams.
//!
//! GB28181 cameras send RTP packets whose payload is MPEG-PS data
//! containing PES-packaged H.264/H.265 video and audio streams.
//! This module demuxes PS data to extract raw NAL units.

use bytes::Bytes;

/// Demuxed PES packet content.
#[derive(Debug, Clone)]
pub struct PesPacket {
    /// PES stream_id (0xE0 = video, 0xC0 = audio).
    pub stream_id: u8,
    /// Raw PES payload (NAL units for video).
    pub data: Bytes,
    /// PTS in 90kHz ticks (if present in PES header).
    pub pts: Option<u64>,
}

/// Error from PS parsing.
#[derive(Debug)]
pub enum PsError {
    Incomplete,
    InvalidStartCode,
}

/// Parses a single PES packet from a buffer of PS data.
///
/// Returns `Ok(Some(PesPacket, consumed_bytes))` on success,
/// `Ok(None)` if data is incomplete, or `Err` on format error.
pub fn parse_one_pes(data: &[u8]) -> Result<Option<(PesPacket, usize)>, PsError> {
    if data.len() < 6 {
        return Ok(None);
    }

    // Skip PS pack headers (0xBA), system headers (0xBB), map (0xBC)
    let mut pos = 0;
    loop {
        if pos + 4 > data.len() {
            return Ok(None);
        }
        let code = &data[pos..];
        if code.len() < 4 || code[0] != 0x00 || code[1] != 0x00 || code[2] != 0x01 {
            return Err(PsError::InvalidStartCode);
        }
        let sc = code[3];
        match sc {
            0xBA => {
                // PS pack header: skip fixed 14 bytes
                if pos + 14 > data.len() {
                    return Ok(None);
                }
                // Check stuffing length at byte 13
                let stuff_len = (data[pos + 13] & 0x07) as usize;
                pos += 14 + stuff_len;
            }
            0xBB => {
                // System header: skip (read header length at byte 6)
                if pos + 6 > data.len() {
                    return Ok(None);
                }
                let hdr_len = u16::from_be_bytes([data[pos + 4], data[pos + 5]]) as usize;
                pos += 6 + hdr_len;
            }
            0xBC => {
                // Program Stream Map: skip
                if pos + 6 > data.len() {
                    return Ok(None);
                }
                let psm_len = u16::from_be_bytes([data[pos + 4], data[pos + 5]]) as usize;
                pos += 6 + psm_len;
            }
            _ => {
                // Should be PES (0xC0-0xEF for audio/video)
                if sc >= 0xC0 {
                    return parse_pes_packet(&data[pos..]).map(|opt| {
                        opt.map(|(pkt, consumed)| (pkt, pos + consumed))
                    });
                }
                // Unknown start code — skip past it
                pos += 4;
                // Scan to next start code
                while pos + 4 <= data.len() {
                    if data[pos] == 0x00
                        && data[pos + 1] == 0x00
                        && data[pos + 2] == 0x01
                    {
                        break;
                    }
                    pos += 1;
                }
            }
        }
    }
}

/// Parse a single PES packet starting at `data[0..]`.
/// Returns `(PesPacket, consumed_bytes)` or `None` if incomplete.
fn parse_pes_packet(data: &[u8]) -> Result<Option<(PesPacket, usize)>, PsError> {
    if data.len() < 9 {
        return Ok(None);
    }
    // data[0..3] = 00 00 01, data[3] = stream_id
    let stream_id = data[3];
    let pes_packet_len = u16::from_be_bytes([data[4], data[5]]) as usize;

    if data.len() < 6 + pes_packet_len {
        return Ok(None);
    }

    let mut pos = 6; // after packet length field

    // Check PTS/DTS flags
    let pts_dts_flags = if pos < data.len() {
        (data[pos] >> 6) & 0x03
    } else {
        0
    };
    let pes_hdr_data_len = if pos < data.len() {
        data[pos + 1] as usize
    } else {
        0
    };

    // Skip flags + PES header data length byte
    pos += 2;
    let hdr_end = pos + pes_hdr_data_len;

    let mut pts = None;
    if pts_dts_flags >= 2 && pos + 5 <= data.len() && hdr_end <= data.len() {
        // PTS present (33 bits)
        let b0 = data[pos] as u64;
        let b1 = data[pos + 1] as u64;
        let b2 = data[pos + 2] as u64;
        let b3 = data[pos + 3] as u64;
        let b4 = data[pos + 4] as u64;
        pts = Some(
            ((b0 & 0x0E) << 29)
                | ((b1 & 0xFF) << 22)
                | ((b2 & 0xFE) << 14)
                | ((b3 & 0xFF) << 7)
                | ((b4 & 0xFE) >> 1),
        );
    }

    pos = hdr_end;

    // PES payload follows
    let payload_end = 6 + pes_packet_len;
    if payload_end > data.len() {
        return Ok(None);
    }
    let payload = Bytes::copy_from_slice(&data[pos..payload_end]);

    Ok(Some((PesPacket { stream_id, data: payload, pts }, payload_end)))
}

/// Extracts H.264/H.265 NAL units from a PES video payload.
/// Handles both Annex-B and length-prefixed formats.
pub fn extract_nalus_from_pes(payload: &[u8]) -> Vec<Bytes> {
    let mut nalus = Vec::new();

    // Try Annex-B format first (start-code delimited)
    let mut pos = 0;
    let mut found_annexb = false;
    while pos + 4 <= payload.len() {
        if payload[pos] == 0x00
            && payload[pos + 1] == 0x00
            && (payload[pos + 2] == 0x01
                || (payload[pos + 2] == 0x00 && pos + 3 < payload.len() && payload[pos + 3] == 0x01))
        {
            found_annexb = true;
            let sc_start = pos;
            // Find next start code
            let sc_len = if payload[pos + 2] == 0x01 { 3 } else { 4 };
            pos += sc_len;
            let nalu_start = pos;
            while pos + 4 <= payload.len() {
                if payload[pos] == 0x00
                    && payload[pos + 1] == 0x00
                    && (payload[pos + 2] == 0x01
                        || (payload[pos + 2] == 0x00
                            && pos + 3 < payload.len()
                            && payload[pos + 3] == 0x01))
                {
                    break;
                }
                pos += 1;
            }
            // Include the start code in the NALU
            let mut nalu = Vec::with_capacity((pos - sc_start) + 4);
            nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            nalu.extend_from_slice(&payload[nalu_start..pos]);
            nalus.push(Bytes::from(nalu));
        } else {
            pos += 1;
        }
    }

    if found_annexb {
        return nalus;
    }

    // Try length-prefixed format (GB28181 H.264 encapsulation)
    // Format: [00 00 00 01] then length-prefixed NALUs
    if payload.len() >= 5
        && payload[0] == 0x00
        && payload[1] == 0x00
        && payload[2] == 0x00
        && payload[3] == 0x01
    {
        let data = &payload[4..];
        pos = 0;
        while pos + 2 <= data.len() {
            let nalu_len = if pos + 4 <= data.len() {
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize
            } else if pos + 2 <= data.len() {
                u16::from_be_bytes([data[pos], data[pos + 1]]) as usize
            } else {
                break;
            };
            let prefix_len = if pos + 4 <= data.len() { 4 } else { 2 };
            pos += prefix_len;
            if pos + nalu_len > data.len() {
                break;
            }
            let mut nalu = Vec::with_capacity(nalu_len + 4);
            nalu.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            nalu.extend_from_slice(&data[pos..pos + nalu_len]);
            nalus.push(Bytes::from(nalu));
            pos += nalu_len;
        }
    }

    nalus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pes_video_basic() {
        // Minimal PS pack + PES video packet
        let mut data = Vec::new();
        // PS pack header (14 bytes)
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        data.extend_from_slice(&[0x44, 0x00, 0x04, 0x00, 0x04, 0x01]); // SCR fields (6 bytes)
        data.extend_from_slice(&[0x01, 0x02, 0x03, 0xF8]); // mux_rate(3) + stuffing_len=0
        // PES video
        let pes_payload = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x20, 0x30];
        let pes_len = pes_payload.len() + 3; // +3 for PTS_DTS flags + hdr_len + flags
        data.push(0x00);
        data.push(0x00);
        data.push(0x01);
        data.push(0xE0); // stream_id = video
        data.extend_from_slice(&(pes_len as u16).to_be_bytes());
        // Flags: PTS present, PES header data length = 5
        data.push(0x80); // PTS only
        data.push(0x05); // PES hdr data len = 5
        // PTS (5 bytes)
        data.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]);
        // Payload
        data.extend_from_slice(&pes_payload);

        let (pkt, consumed) = parse_one_pes(&data).unwrap().unwrap();
        assert!(consumed > 0);
        assert_eq!(pkt.stream_id, 0xE0);
        assert!(!pkt.data.is_empty());

        let nalus = extract_nalus_from_pes(&pkt.data);
        assert!(!nalus.is_empty());
    }

    #[test]
    fn extract_nalus_annexb() {
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x11,
            0x00, 0x00, 0x00, 0x01, 0x21, 0x30, 0x31,
        ];
        let nalus = extract_nalus_from_pes(&data);
        assert_eq!(nalus.len(), 2);
    }

    #[test]
    fn ps_header_skip() {
        // Just a PS pack header — should return None (incomplete, no PES)
        let data = [
            0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00,
            0x04, 0x01, 0x01, 0x02, 0x03, 0xFE,
        ];
        let result = parse_one_pes(&data);
        assert!(matches!(result, Ok(None)));
    }
}
