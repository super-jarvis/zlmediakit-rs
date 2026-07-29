use std::collections::HashMap;

use bytes::{Buf, Bytes, BytesMut};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};

/// Maps an MPEG-TS `stream_type` to a `CodecId`.
fn stream_type_to_codec(stream_type: u8) -> Option<CodecId> {
    match stream_type {
        0x1B => Some(CodecId::H264),        // AVC video
        0x24 => Some(CodecId::H265),        // HEVC video
        0x06 | 0x0F => Some(CodecId::AAC),  // private (ADTS) / AAC
        0x03 | 0x04 => Some(CodecId::MP2A), // MPEG-2 audio
        _ => None,
    }
}

pub struct HlsDemuxer {
    buffer: BytesMut,
    /// PID of the PMT, learned from the PAT.
    pmt_pid: Option<u16>,
    /// elementary_PID -> codec, learned from the PMT.
    pid_codecs: HashMap<u16, CodecId>,
    /// PSI section reassembly buffer keyed by PID (PAT/PMT can span packets).
    psi: HashMap<u16, BytesMut>,
}

impl HlsDemuxer {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(4096),
            pmt_pid: None,
            pid_codecs: HashMap::new(),
            psi: HashMap::new(),
        }
    }

    pub fn parse_m3u8(&self, data: &[u8]) -> Option<Vec<String>> {
        let content = std::str::from_utf8(data).ok()?;
        let mut segments = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            segments.push(line.to_string());
        }

        Some(segments)
    }

    pub fn feed_ts(&mut self, data: &[u8]) -> Vec<MediaFrame> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();

        while self.buffer.len() >= 188 {
            if self.buffer[0] != 0x47 {
                self.buffer.advance(1);
                continue;
            }

            let pusi = self.buffer[1] & 0x40 != 0;
            let pid = (((self.buffer[1] as u16) & 0x1F) << 8) | (self.buffer[2] as u16);
            let afc = (self.buffer[3] >> 4) & 0x03;

            // Locate the payload start, honoring the adaptation field.
            let mut pos = 4;
            if afc & 0x02 != 0 {
                let af_len = self.buffer[4] as usize;
                pos = 5 + af_len;
            }
            // A PUSI packet carries a pointer_field before the section/PES start.
            if pusi && pos < 188 {
                let pointer = self.buffer[pos] as usize;
                pos += 1 + pointer;
            }

            if pos < 188 {
                let payload = self.buffer[pos..188].to_vec();
                // PAT (PID 0): learn the PMT PID.
                if pid == 0 {
                    self.ingest_psi(pid, pusi, &payload);
                    self.parse_pat();
                } else if Some(pid) == self.pmt_pid {
                    self.ingest_psi(pid, pusi, &payload);
                    self.parse_pmt();
                } else if let Some(codec) = self.pid_codecs.get(&pid).copied() {
                    // PES data for a known elementary stream.
                    let is_video = codec.is_video();
                    let payload = Bytes::from(payload);
                    let frame = if is_video {
                        MediaFrame::new_video(0, codec, 0, 0, 0, payload, true)
                    } else {
                        MediaFrame::new_audio(1, codec, 0, 0, 0, payload)
                    };
                    frames.push(frame);
                }
                // For PIDs we have not yet mapped (PMT not seen yet), drop until
                // the PMT has been parsed.
            }

            self.buffer.advance(188);
        }

        frames
    }

    /// Appends PSI payload bytes, reassembling across packets. Returns the
    /// complete section bytes when a section boundary is reached.
    fn ingest_psi(&mut self, pid: u16, pusi: bool, payload: &[u8]) -> Option<Vec<u8>> {
        let entry = self.psi.entry(pid).or_default();
        if pusi {
            entry.clear();
        }
        entry.extend_from_slice(payload);

        // A PSI section's section_length field (excluding the 3-byte prefix)
        // begins at offset 1. We can only parse once enough bytes are buffered.
        if entry.len() < 3 + 3 {
            return None;
        }
        let section_length = (((entry[1] & 0x0F) as usize) << 8) | (entry[2] as usize);
        let total = 3 + section_length;
        if entry.len() >= total {
            Some(entry[..total].to_vec())
        } else {
            None
        }
    }

    fn parse_pat(&mut self) {
        let Some(section) = self.psi.get(&0).cloned() else {
            return;
        };
        if section.len() < 8 || section[0] != 0x00 {
            return;
        }
        let section_length = (((section[1] & 0x0F) as usize) << 8) | (section[2] as usize);
        // section: [table_id(1)] [syntax+length(2)] [ts_id(2)] [version(1)]
        // [sec_num(1)] [last_sec(1)] then N*(program_number(2)+pid(2)).
        let end = 3 + section_length;
        let mut i = 8;
        while i + 4 <= end && i + 4 <= section.len() {
            let program_number = ((section[i] as u16) << 8) | (section[i + 1] as u16);
            let pmt_pid = ((section[i + 2] as u16) & 0x1F) << 8 | (section[i + 3] as u16);
            // program_number == 0 is the network PID, not a PMT.
            if program_number != 0 {
                self.pmt_pid = Some(pmt_pid);
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self) {
        let Some(pmt_pid) = self.pmt_pid else {
            return;
        };
        let Some(section) = self.psi.get(&pmt_pid).cloned() else {
            return;
        };
        if section.len() < 12 || section[0] != 0x02 {
            return;
        }
        let section_length = (((section[1] & 0x0F) as usize) << 8) | (section[2] as usize);
        let end = (3 + section_length).min(section.len());
        // Skip: table_id(1) syntax+len(2) program_number(2) version(1)
        // sec_num(1) last_sec(1) pcr_pid(2) prog_info_len(2).
        let mut i = 10;
        if i + 2 > end {
            return;
        }
        let prog_info_len = (((section[i] & 0x0F) as usize) << 8) | (section[i + 1] as usize);
        i += 2 + prog_info_len;
        // Loop over elementary streams: stream_type(1) pid(2) es_info_len(2)+info.
        while i + 5 <= end {
            let stream_type = section[i];
            let e_pid = (((section[i + 1] as u16) & 0x1F) << 8) | (section[i + 2] as u16);
            let es_info_len = (((section[i + 3] as usize) & 0x0F) << 8) | (section[i + 4] as usize);
            if let Some(codec) = stream_type_to_codec(stream_type) {
                self.pid_codecs.insert(e_pid, codec);
            }
            i += 5 + es_info_len;
        }
    }
}

impl Default for HlsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 188-byte TS packet carrying `payload` on `pid`.
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> [u8; 188] {
        let mut p = [0x47u8; 188];
        let mut b1 = (pid >> 8 & 0x1F) as u8;
        if pusi {
            b1 |= 0x40;
        }
        p[1] = b1;
        p[2] = (pid & 0xFF) as u8;
        p[3] = 0x10; // afc = 0x1 (payload only), no adaptation field.
                     // pointer_field = 0 for PUSI PSI packets.
        p[4] = 0;
        let n = payload.len().min(183);
        p[5..5 + n].copy_from_slice(&payload[..n]);
        p
    }

    /// Minimal PAT pointing program 1 -> PMT PID 0x1000 (no CRC checked by parser).
    fn pat() -> Vec<u8> {
        vec![
            0x00, 0x80, 0x09, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ]
    }

    /// Minimal PMT on PID 0x1000: H265 on 0x0100, AAC on 0x0101.
    fn pmt() -> Vec<u8> {
        vec![
            // table_id, section_syntax(0x80|0x01), section_length=0x13(19)
            0x02, 0x81, 0x13, // program_number(2), version, sec_num, last_sec
            0x00, 0x01, 0xC1, 0x00, 0x00, // pcr_pid (0xE0 0x00), prog_info_len (0 0)
            0xE0, 0x00, 0x00, 0x00, // stream entry: H265
            0x24, 0xE1, 0x00, 0x00, 0x00, // stream entry: AAC
            0x0F, 0xE1, 0x01, 0x00, 0x00,
        ]
    }

    #[test]
    fn pmt_identifies_hevc_and_aac() {
        let mut d = HlsDemuxer::new();
        // PAT -> learns PMT PID.
        let frames = d.feed_ts(&ts_packet(0, true, &pat()));
        assert!(frames.is_empty());
        assert_eq!(d.pmt_pid, Some(0x1000));
        // PMT -> learns elementary PIDs.
        let frames = d.feed_ts(&ts_packet(0x1000, true, &pmt()));
        assert!(frames.is_empty());
        assert_eq!(d.pid_codecs.get(&0x0100), Some(&CodecId::H265));
        assert_eq!(d.pid_codecs.get(&0x0101), Some(&CodecId::AAC));
        // A video PES on the HEVC PID is emitted with the correct codec.
        let frames = d.feed_ts(&ts_packet(0x0100, true, &[0xAA, 0xBB, 0xCC]));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].codec, CodecId::H265);
        assert!(frames[0].frame_type == zlmediakit_core::media_frame::FrameType::Video);
    }

    #[test]
    fn unknown_pid_dropped_before_pmt() {
        let mut d = HlsDemuxer::new();
        // Video PID 0x0100 has no mapping yet -> dropped, no frames.
        let frames = d.feed_ts(&ts_packet(0x0100, true, &[0x01, 0x02]));
        assert!(frames.is_empty());
    }
}
