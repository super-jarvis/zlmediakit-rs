use std::collections::VecDeque;
use crate::media_frame::{FrameType, MediaFrame};
use tracing::debug;

#[derive(Clone, Debug)]
pub struct CachedFrame {
    pub frame: MediaFrame,
}

#[derive(Clone, Debug)]
pub struct GopCache {
    metadata: Option<Vec<CachedFrame>>,
    video_config: Option<CachedFrame>,
    audio_config: Option<CachedFrame>,
    frames: VecDeque<CachedFrame>,
    max_gop_count: usize,
}

impl GopCache {
    pub fn new() -> Self {
        Self {
            metadata: None,
            video_config: None,
            audio_config: None,
            frames: VecDeque::new(),
            max_gop_count: 1,
        }
    }

    pub fn clear(&mut self) {
        self.metadata = None;
        self.video_config = None;
        self.audio_config = None;
        self.frames.clear();
    }

    pub fn cache_frame(&mut self, frame: &MediaFrame) {
        if frame.frame_type == FrameType::Metadata {
            self.metadata = Some(vec![CachedFrame { frame: frame.clone() }]);
            return;
        }

        if frame.frame_type == FrameType::Video && is_config_frame(frame) {
            self.video_config = Some(CachedFrame { frame: frame.clone() });
            return;
        }

        if frame.frame_type == FrameType::Audio && is_config_frame(frame) {
            self.audio_config = Some(CachedFrame { frame: frame.clone() });
            return;
        }

        if frame.frame_type == FrameType::Video && frame.key_frame {
            while self.frames.len() >= self.max_gop_count * 30 {
                self.frames.pop_front();
            }
        }

        self.frames.push_back(CachedFrame { frame: frame.clone() });
    }

    pub fn get_all_frames(&self) -> Vec<MediaFrame> {
        let mut result = Vec::new();

        if let Some(ref meta) = self.metadata {
            for f in meta {
                result.push(f.frame.clone());
            }
        }
        if let Some(ref vc) = self.video_config {
            result.push(vc.frame.clone());
        }
        if let Some(ref ac) = self.audio_config {
            result.push(ac.frame.clone());
        }
        for f in &self.frames {
            result.push(f.frame.clone());
        }

        result
    }

    pub fn get_config_frames(&self) -> Vec<MediaFrame> {
        let mut result = Vec::new();
        if let Some(ref vc) = self.video_config {
            result.push(vc.frame.clone());
        }
        if let Some(ref ac) = self.audio_config {
            result.push(ac.frame.clone());
        }
        result
    }

    pub fn get_latest_gop_frames(&self) -> Vec<MediaFrame> {
        let mut result = Vec::new();

        if let Some(ref meta) = self.metadata {
            for f in meta {
                result.push(f.frame.clone());
            }
        }
        if let Some(ref vc) = self.video_config {
            result.push(vc.frame.clone());
        }
        if let Some(ref ac) = self.audio_config {
            result.push(ac.frame.clone());
        }

        let mut start_idx = self.frames.len();
        for (i, f) in self.frames.iter().enumerate() {
            if f.frame.frame_type == FrameType::Video && f.frame.key_frame {
                start_idx = i;
            }
        }
        for f in self.frames.iter().skip(start_idx) {
            result.push(f.frame.clone());
        }

        debug!("GopCache get_latest: returning={} frames", result.len());
        result
    }
}

impl Default for GopCache {
    fn default() -> Self {
        Self::new()
    }
}

pub fn is_config_frame(frame: &MediaFrame) -> bool {
    if frame.frame_type == FrameType::Video {
        if frame.data.len() >= 2 {
            let codec_id = frame.data[0] & 0x0F;
            if codec_id == 7 && frame.data.len() >= 2 {
                let packet_type = frame.data[1] & 0x0F;
                return packet_type == 0;
            }
            if codec_id == 12 && frame.data.len() >= 5 {
                let packet_type = frame.data[1] & 0x0F;
                return packet_type == 0;
            }
        }
    }
    if frame.frame_type == FrameType::Audio && frame.data.len() >= 2 {
        let sound_format = (frame.data[0] >> 4) & 0x0F;
        if sound_format == 10 {
            let packet_type = frame.data[1] & 0x0F;
            return packet_type == 0;
        }
    }
    false
}
