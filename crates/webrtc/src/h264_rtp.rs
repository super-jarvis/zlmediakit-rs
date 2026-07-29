//! Minimal H.264 RTP depacketizer (RFC 6184) used by the WHIP receiver.
//!
//! The `webrtc` crate only exposes raw `rtp::packet::Packet`s on the receive
//! side (the `rtp` 0.12 it depends on ships no H.264 *de*payloader), so we
//! reassemble frames here. Single-Nalu, STAP-A aggregation, and FU-A
//! fragmentation are handled. The output is Annex-B (start-code separated
//! NALUs), which the WHIP layer converts to the AVCC layout the rest of the
//! pipeline expects.

use bytes::{Bytes, BytesMut};

const NALU_TYPE_MASK: u8 = 0x1F;
const FU_A_TYPE: u8 = 28;
const STAP_A_TYPE: u8 = 24;
const FU_START: u8 = 0x80;
const FU_END: u8 = 0x40;
const START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// Reassembles H.264 RTP packets into Annex-B encoded frames.
pub struct H264RtpDepacketizer {
    fu_nalu: BytesMut,
    fu_active: bool,
}

impl H264RtpDepacketizer {
    pub fn new() -> Self {
        Self {
            fu_nalu: BytesMut::new(),
            fu_active: false,
        }
    }

    /// Feed one RTP payload. Returns `Some(annexb_frame)` when a complete
    /// encoded frame (one or more NALUs) has been reassembled.
    pub fn push(&mut self, payload: &[u8]) -> Option<Bytes> {
        if payload.is_empty() {
            return None;
        }
        let nalu_header = payload[0];
        let nalu_type = nalu_header & NALU_TYPE_MASK;

        if nalu_type == FU_A_TYPE {
            // FU-A fragmentation unit: [FU indicator | FU header | data...]
            if payload.len() < 2 {
                return None;
            }
            let fu_header = payload[1];
            let start = fu_header & FU_START != 0;
            let end = fu_header & FU_END != 0;
            if start {
                // Reconstruct the original NALU header:
                //   (FU indicator & 0xE0) | (FU header & 0x1F)
                let reconstructed = (nalu_header & 0xE0) | (fu_header & NALU_TYPE_MASK);
                self.fu_nalu.clear();
                self.fu_nalu.extend_from_slice(START_CODE);
                self.fu_nalu.push(reconstructed);
                self.fu_nalu.extend_from_slice(&payload[2..]);
                self.fu_active = true;
            } else if self.fu_active {
                self.fu_nalu.extend_from_slice(&payload[2..]);
            } else {
                // Fragment without a preceding start; discard.
                return None;
            }
            if end {
                self.fu_active = false;
                let out = self.fu_nalu.split().freeze();
                return Some(out);
            }
            None
        } else if nalu_type == STAP_A_TYPE {
            // STAP-A: one packet carrying several NALUs, each 2-byte length prefixed.
            let mut out = BytesMut::new();
            let mut pos = 1; // skip the STAP-A header byte
            while pos + 2 <= payload.len() {
                let nalu_len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
                pos += 2;
                if pos + nalu_len > payload.len() {
                    break;
                }
                out.extend_from_slice(START_CODE);
                out.extend_from_slice(&payload[pos..pos + nalu_len]);
                pos += nalu_len;
            }
            if out.is_empty() {
                None
            } else {
                Some(out.freeze())
            }
        } else {
            // Single NALU packet (types 1..=23, including SPS=7 / PPS=8).
            let mut out = BytesMut::new();
            out.extend_from_slice(START_CODE);
            out.extend_from_slice(payload);
            Some(out.freeze())
        }
    }
}

impl Default for H264RtpDepacketizer {
    fn default() -> Self {
        Self::new()
    }
}
