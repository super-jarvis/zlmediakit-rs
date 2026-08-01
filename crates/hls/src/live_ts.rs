//! Live MPEG-TS muxer for HTTP-TS / WebSocket-TS streaming.
//!
//! Unlike HLS (which buffers into segment files), this muxer emits a
//! continuous TS byte stream: one PAT/PMT pair when the stream starts (once the
//! video codec is known), then a PES packet per media frame. Every `push_frame`
//! returns exactly the bytes that should be handed to the client.

use zlmediakit_core::media_frame::{CodecId, FrameType, MediaFrame};

use crate::muxer::{
    build_config_pes, build_data_pes_raw, extract_aac_config, extract_video_config,
    flv_audio_to_adts, flv_audio_to_raw, flv_video_to_annex_b, is_aac_config, is_video_config,
    TsWriter, AUDIO_PID, VIDEO_PID,
};

/// Continuously muxes frames into MPEG-TS packets.
pub struct TsLiveMuxer {
    writer: TsWriter,
    started: bool,
    video_codec: Option<CodecId>,
    video_config_data: Option<Vec<u8>>,
    video_config_sent: bool,
    audio_config_data: Option<Vec<u8>>,
    audio_config_sent: bool,
}

impl Default for TsLiveMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsLiveMuxer {
    pub fn new() -> Self {
        Self {
            writer: TsWriter::new(),
            started: false,
            video_codec: None,
            video_config_data: None,
            video_config_sent: false,
            audio_config_data: None,
            audio_config_sent: false,
        }
    }

    /// Mux one frame. Returns the TS bytes to send (may be empty for frames
    /// that carry no muxable payload).
    pub fn push_frame(&mut self, frame: &MediaFrame) -> Vec<u8> {
        let mut out = Vec::new();

        // Capture config data and learn the codec.
        match frame.frame_type {
            FrameType::Video => {
                self.video_codec = Some(frame.codec);
                if is_video_config(frame) {
                    self.video_config_data = Some(extract_video_config(frame));
                    self.video_config_sent = false;
                }
            }
            FrameType::Audio if is_aac_config(frame) => {
                self.audio_config_data = Some(extract_aac_config(&frame.data));
                self.audio_config_sent = false;
            }
            _ => {}
        }

        // PAT/PMT once, as soon as we have anything to describe.
        if !self.started {
            let video_stream_type = match self.video_codec {
                Some(CodecId::H265) => 0x24,
                _ => 0x1B,
            };
            self.writer.write_pat_pmt(&mut out, video_stream_type);
            self.started = true;
        }

        match frame.frame_type {
            FrameType::Video => {
                if !self.video_config_sent {
                    if let Some(ref cfg) = self.video_config_data {
                        let pes = build_config_pes(0xE0, cfg);
                        self.writer
                            .write_pes(&mut out, VIDEO_PID, &pes, true, frame.dts);
                        self.video_config_sent = true;
                    }
                }
                if !is_video_config(frame) {
                    let annex_b = flv_video_to_annex_b(&frame.data);
                    if !annex_b.is_empty() {
                        let pes = build_data_pes_raw(0xE0, &annex_b, frame.timestamp);
                        self.writer.write_pes(
                            &mut out,
                            VIDEO_PID,
                            &pes,
                            frame.key_frame,
                            frame.dts,
                        );
                    }
                }
            }
            FrameType::Audio => {
                if !self.audio_config_sent {
                    if let Some(ref cfg) = self.audio_config_data {
                        let pes = build_config_pes(0xC0, cfg);
                        self.writer
                            .write_pes(&mut out, AUDIO_PID, &pes, true, frame.dts);
                        self.audio_config_sent = true;
                    }
                }
                if !is_aac_config(frame) {
                    let audio = if let Some(ref cfg) = self.audio_config_data {
                        flv_audio_to_adts(&frame.data, cfg)
                    } else {
                        flv_audio_to_raw(&frame.data)
                    };
                    if !audio.is_empty() {
                        let pes = build_data_pes_raw(0xC0, &audio, frame.timestamp);
                        self.writer
                            .write_pes(&mut out, AUDIO_PID, &pes, false, frame.dts);
                    }
                }
            }
            _ => {}
        }

        out
    }

    /// True once PAT/PMT has been emitted.
    pub fn started(&self) -> bool {
        self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn avc_config_frame() -> MediaFrame {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0xD9, 0x01, 0x01];
        let pps = [0x68, 0xCE, 0x0F, 0x10];
        let mut data = vec![0x17, 0x00, 0x00, 0x00, 0x00, 0x01];
        data.push(0xE1);
        data.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        data.extend_from_slice(&sps);
        data.push(0x01);
        data.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        data.extend_from_slice(&pps);
        let mut f = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, Bytes::from(data), false);
        f.config_frame = true;
        f
    }

    fn video_frame(ts: u32, key: bool) -> MediaFrame {
        let nalu = [
            0x00,
            0x00,
            0x00,
            0x01,
            if key { 0x65 } else { 0x41 },
            0x88,
            0x84,
        ];
        let mut data = vec![if key { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
        data.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        data.extend_from_slice(&nalu);
        MediaFrame::new_video(
            0,
            CodecId::H264,
            ts,
            ts as u64,
            ts as u64,
            Bytes::from(data),
            key,
        )
    }

    fn aac_config_frame() -> MediaFrame {
        // AAC-HE LC, 44.1kHz, stereo ASC in FLV config layout.
        let data = vec![0xAF, 0x00, 0x00, 0x00, 0x00, 0x12, 0x10];
        let mut f = MediaFrame::new_audio(0, CodecId::AAC, 0, 0, 0, Bytes::from(data));
        f.config_frame = true;
        f
    }

    fn aac_frame(ts: u32) -> MediaFrame {
        let data = vec![0xAF, 0x01, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04];
        MediaFrame::new_audio(0, CodecId::AAC, ts, ts as u64, ts as u64, Bytes::from(data))
    }

    #[test]
    fn emits_pat_pmt_and_pes() {
        let mut muxer = TsLiveMuxer::new();
        let out1 = muxer.push_frame(&avc_config_frame());
        assert!(!out1.is_empty(), "config frame must produce TS bytes");
        // First packet must be the sync byte + PAT/PMT.
        assert_eq!(out1[0], 0x47);
        // PAT/PMT present: second and third 188-byte packets carry the tables.
        assert_eq!(out1[188], 0x47);
        assert_eq!(out1[376], 0x47);

        let out2 = muxer.push_frame(&video_frame(40, true));
        assert!(!out2.is_empty());
        // All output is a whole number of 188-byte packets.
        assert_eq!(out2.len() % 188, 0);
        assert_eq!(out1.len() % 188, 0);

        let out3 = muxer.push_frame(&aac_config_frame());
        let out4 = muxer.push_frame(&aac_frame(46));
        assert!(!out3.is_empty());
        assert!(!out4.is_empty());
        assert_eq!(out3.len() % 188, 0);
        assert_eq!(out4.len() % 188, 0);
    }

    #[test]
    fn no_pat_pmt_repeat_after_start() {
        let mut muxer = TsLiveMuxer::new();
        let first = muxer.push_frame(&avc_config_frame());
        let second = muxer.push_frame(&video_frame(40, true));
        // The first frame carries PAT+PMT (3 packets); later frames must not.
        assert_eq!(first.len(), 3 * 188);
        assert!(second.len() < first.len());
    }
}
