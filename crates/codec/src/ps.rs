//! MPEG-PS (Program Stream) parser for GB28181 RTP/PS streams.
//!
//! GB28181 cameras send RTP packets whose payload is MPEG-PS data
//! containing PES-packaged H.264/H.265 video and audio streams.
//! This module demuxes PS data to extract raw NAL units.

use bytes::{Buf, Bytes, BytesMut};

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

/// Incremental MPEG-PS demuxer.
///
/// GB28181 cameras do not reliably set the RTP marker bit, and a single PES
/// packet can be spread across several RTP packets. This demuxer accumulates
/// raw PS bytes and yields complete [`PesPacket`]s as they become available,
/// so the caller no longer has to wait for a marker before parsing.
pub struct PsDemuxer {
    buf: BytesMut,
    /// Max buffer size before we assume the stream is malformed and resync.
    max_buf: usize,
}

impl PsDemuxer {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
            max_buf: 4 * 1024 * 1024,
        }
    }

    /// Appends RTP payload bytes to the internal buffer.
    pub fn push(&mut self, data: &[u8]) {
        if self.buf.len() + data.len() > self.max_buf {
            // Drop a bounded prefix to avoid unbounded growth; keep the
            // newest bytes since the tail of the buffer is most likely part
            // of the PES being accumulated.
            let drop = self.buf.len() + data.len() - self.max_buf;
            let drop = drop.min(self.buf.len());
            self.buf.advance(drop);
        }
        self.buf.extend_from_slice(data);
    }

    /// Attempts to extract the next complete PES packet. Returns `None` when
    /// more data is required or the buffer is malformed (in which case it
    /// resyncs to the next start code).
    pub fn next_pes(&mut self) -> Option<PesPacket> {
        loop {
            match parse_one_pes(&self.buf) {
                Ok(Some((pes, consumed))) => {
                    self.buf.advance(consumed);
                    return Some(pes);
                }
                Ok(None) => {
                    // No complete PES yet. Drop leading bytes that already form
                    // complete PS headers (pack/system/map), so a header-only
                    // buffer does not grow without bound between PES packets.
                    let mut pos = 0;
                    while let Some(n) = leading_header_len(&self.buf[pos..]) {
                        pos += n;
                    }
                    if pos > 0 {
                        self.buf.advance(pos);
                        continue;
                    }
                    return None;
                }
                Err(_) => {
                    // Resync: drop bytes up to the next entity start code.
                    match find_start_code(&self.buf, 0) {
                        Some(sc) => self.buf.advance(sc.max(1)),
                        None => {
                            self.buf.clear();
                            return None;
                        }
                    }
                }
            }
        }
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

impl Default for PsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Finds the offset of the next start code that begins a new PS entity
/// (pack header, system header, PSM or a PES packet) at or after `from`.
///
/// The scan is entity-aware: NAL start codes (e.g. `00 00 00 01 09` for
/// H.264) inside a PES payload are skipped, since their 4th byte is below
/// `0xB9`, while every PS entity start code has an id >= `0xB9`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 <= data.len() {
        // 3-byte start code: 00 00 01 <id>
        if data[i] == 0x00 && data[i + 1] == 0x00 && data[i + 2] == 0x01 {
            if is_ps_entity(data[i + 3]) {
                return Some(i);
            }
            i += 4;
            continue;
        }
        // 4-byte start code: 00 00 00 01 <id>
        if i + 5 <= data.len()
            && data[i] == 0x00
            && data[i + 1] == 0x00
            && data[i + 2] == 0x00
            && data[i + 3] == 0x01
            && is_ps_entity(data[i + 4])
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Whether a start-code id identifies a PS entity rather than ES (NAL) data.
fn is_ps_entity(id: u8) -> bool {
    id >= 0xB9
}

/// Returns the byte length of a complete leading PS pack/system/map header,
/// or `None` if `data` does not begin with one (or it is incomplete).
fn leading_header_len(data: &[u8]) -> Option<usize> {
    if data.len() < 4 || data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return None;
    }
    let n = match data[3] {
        0xBA => {
            if data.len() < 14 {
                return None;
            }
            let stuff = (data[13] & 0x07) as usize;
            if data.len() < 14 + stuff {
                return None;
            }
            14 + stuff
        }
        0xBB | 0xBC => {
            if data.len() < 6 {
                return None;
            }
            let len = u16::from_be_bytes([data[4], data[5]]) as usize;
            if data.len() < 6 + len {
                return None;
            }
            6 + len
        }
        _ => return None,
    };
    Some(n)
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
                // Unknown start code — skip past it and resync to the next entity
                pos += 4;
                match find_start_code(data, pos) {
                    Some(sc) => pos = sc,
                    None => return Ok(None),
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

    if pes_packet_len != 0 && data.len() < 6 + pes_packet_len {
        return Ok(None);
    }

    let mut pos = 6; // after packet length field

    // Check PTS/DTS flags
    let pts_dts_flags = if pos < data.len() {
        (data[pos] >> 6) & 0x03
    } else {
        0
    };
    let pes_hdr_data_len = if pos + 1 < data.len() {
        data[pos + 1] as usize
    } else {
        0
    };

    // Skip flags + PES header data length byte
    pos += 2;
    let hdr_end = pos + pes_hdr_data_len;
    if hdr_end > data.len() {
        return Ok(None);
    }

    let mut pts = None;
    if pts_dts_flags >= 2 && pos + 5 <= data.len() {
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

    // PES payload follows. When pes_packet_len == 0 (common for GB28181 video),
    // the length is unspecified and the payload runs until the next start code.
    let payload_end = if pes_packet_len == 0 {
        match find_start_code(data, pos) {
            Some(sc) if sc > pos => sc,
            _ => return Ok(None), // no complete payload yet
        }
    } else {
        let end = 6 + pes_packet_len;
        if end > data.len() {
            return Ok(None);
        }
        end
    };

    if pos >= payload_end {
        return Ok(Some((PesPacket { stream_id, data: Bytes::new(), pts }, payload_end)));
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
        let pes_len = 7 + pes_payload.len(); // flags(1) + hdr_len(1) + PTS(5) + payload
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

    fn build_pes(stream_id: u8, pes_packet_len: u16, payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0x00, 0x00, 0x01, stream_id];
        data.extend_from_slice(&pes_packet_len.to_be_bytes());
        data.push(0x80); // PTS only
        data.push(0x05); // PES hdr data len = 5
        data.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x01]); // PTS
        data.extend_from_slice(payload);
        data
    }

    const PACK_HEADER: [u8; 14] = [0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x02, 0x03, 0xF8];

    #[test]
    fn pes_packet_len_zero() {
        // pes_packet_len == 0 is common for GB28181 video: the payload runs
        // until the next start code.
        let payload1 = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x11, 0x22];
        let payload2 = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x99];
        let mut data = build_pes(0xE0, 0, &payload1);
        data.extend_from_slice(&build_pes(0xE0, 0, &payload2));
        // Terminate the last unterminated PES with a pack header.
        data.extend_from_slice(&PACK_HEADER);

        let (pkt1, consumed) = parse_one_pes(&data).unwrap().unwrap();
        assert_eq!(pkt1.stream_id, 0xE0);
        assert_eq!(pkt1.data.as_ref(), payload1.as_slice());

        let (pkt2, consumed2) = parse_one_pes(&data[consumed..]).unwrap().unwrap();
        assert_eq!(pkt2.data.as_ref(), payload2.as_slice());
        assert_eq!(consumed + consumed2, data.len() - PACK_HEADER.len());
        assert!(pkt1.pts.is_some());
    }

    #[test]
    fn pes_packet_len_zero_incomplete() {
        // Only part of an unterminated PES: must return None (keep buffering)
        // rather than emit a truncated payload.
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x11, 0x22];
        let mut data = build_pes(0xE0, 0, &payload);
        data.push(0x00);
        data.push(0x00);
        let result = parse_one_pes(&data);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn ps_demuxer_incremental() {
        // Feed the buffer byte-by-byte (simulating RTP chunks with no marker
        // bit) and verify all PES packets are recovered in order.
        let mut data = Vec::new();
        data.extend_from_slice(&PACK_HEADER);
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x20, 0x30, 0x40];
        data.extend_from_slice(&build_pes(0xE0, (7 + payload.len()) as u16, &payload));

        let mut demuxer = PsDemuxer::new();
        let mut out = Vec::new();
        for b in data {
            demuxer.push(&[b]);
            while let Some(pes) = demuxer.next_pes() {
                out.push(pes);
            }
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].stream_id, 0xE0);
        assert_eq!(out[0].data.as_ref(), payload.as_slice());
        assert_eq!(demuxer.pending(), 0);
    }

    #[test]
    fn ps_demuxer_len_zero_terminated_by_pack() {
        // A GB28181 camera may send video PES with length == 0; the payload
        // is only complete once the next PS pack header arrives.
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x99, 0xAA];
        let mut data = build_pes(0xE0, 0, &payload);
        // Next pack header marks the PES end.
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x02, 0x03, 0xF8]);

        let mut demuxer = PsDemuxer::new();
        demuxer.push(&data[..10]);
        assert!(demuxer.next_pes().is_none());
        demuxer.push(&data[10..]);
        let pes = demuxer.next_pes().unwrap();
        assert_eq!(pes.data.as_ref(), payload.as_slice());
        assert!(pes.pts.is_some());
        // Drain again so complete trailing headers are consumed.
        assert!(demuxer.next_pes().is_none());
        assert_eq!(demuxer.pending(), 0);
    }

    #[test]
    fn ps_demuxer_two_packets() {
        let payload1 = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x11];
        let payload2 = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x22];
        let mut data = build_pes(0xE0, 0, &payload1);
        data.extend_from_slice(&build_pes(0xE0, 0, &payload2));
        data.extend_from_slice(&PACK_HEADER);

        let mut demuxer = PsDemuxer::new();
        demuxer.push(&data[..7]);
        assert!(demuxer.next_pes().is_none());
        demuxer.push(&data[7..]);
        let mut out = Vec::new();
        while let Some(pes) = demuxer.next_pes() {
            out.push(pes);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data.as_ref(), payload1.as_slice());
        assert_eq!(out[1].data.as_ref(), payload2.as_slice());
    }

    #[test]
    fn ps_demuxer_resync() {
        // Garbage prefix before valid PS data should be skipped.
        let mut data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x01, 0x09, 0xFF];
        data.extend_from_slice(&PACK_HEADER);
        let payload = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02];
        data.extend_from_slice(&build_pes(0xE0, (7 + payload.len()) as u16, &payload));
        data.extend_from_slice(&PACK_HEADER);

        let mut demuxer = PsDemuxer::new();
        demuxer.push(&data);
        let mut out = Vec::new();
        while let Some(pes) = demuxer.next_pes() {
            out.push(pes);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.as_ref(), payload.as_slice());
    }
}
