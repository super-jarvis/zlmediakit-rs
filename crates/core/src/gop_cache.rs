use crate::media_frame::{FrameType, MediaFrame};
use std::collections::VecDeque;
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
            self.metadata = Some(vec![CachedFrame {
                frame: frame.clone(),
            }]);
            return;
        }

        if frame.frame_type == FrameType::Video && is_config_frame(frame) {
            self.video_config = Some(CachedFrame {
                frame: frame.clone(),
            });
            return;
        }

        if frame.frame_type == FrameType::Audio && is_config_frame(frame) {
            self.audio_config = Some(CachedFrame {
                frame: frame.clone(),
            });
            return;
        }

        if frame.frame_type == FrameType::Video && frame.key_frame {
            while self.frames.len() >= self.max_gop_count * 30 {
                self.frames.pop_front();
            }
        }

        self.frames.push_back(CachedFrame {
            frame: frame.clone(),
        });
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
    if frame.frame_type == FrameType::Video && frame.data.len() >= 2 {
        let codec_id = frame.data[0] & 0x0F;
        if codec_id == 7 {
            return (frame.data[1] & 0x0F) == 0;
        }
        if codec_id == 12 && frame.data.len() >= 5 {
            return (frame.data[1] & 0x0F) == 0;
        }
    }
    if frame.frame_type == FrameType::Audio && frame.data.len() >= 2 {
        let sound_format = (frame.data[0] >> 4) & 0x0F;
        if sound_format == 10 {
            return (frame.data[1] & 0x0F) == 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_frame::{CodecId, FrameType, MediaFrame};
    use bytes::Bytes;
    use std::time::Instant;

    fn make_video_frame(
        pts: u32,
        key: bool,
        codec_id: u8,
        packet_type: u8,
        extra: &[u8],
    ) -> MediaFrame {
        let mut data = vec![(key as u8) << 4 | codec_id, packet_type, 0, 0, 0];
        data.extend_from_slice(extra);
        MediaFrame::new_video(
            0,
            CodecId::H264,
            pts,
            pts as u64,
            pts as u64,
            Bytes::from(data),
            key,
        )
    }

    fn make_audio_config() -> MediaFrame {
        let data = vec![0xAF, 0x00]; // AAC sequence header
        MediaFrame {
            track_id: 1,
            frame_type: FrameType::Audio,
            codec: CodecId::AAC,
            timestamp: 0,
            pts: 0,
            dts: 0,
            data: Bytes::from(data),
            payload_format: crate::media_frame::PayloadFormat::Flv,
            key_frame: false,
            config_frame: true,
            received_at: Instant::now(),
        }
    }

    fn make_metadata() -> MediaFrame {
        MediaFrame {
            track_id: 0,
            frame_type: FrameType::Metadata,
            codec: CodecId::H264,
            timestamp: 0,
            pts: 0,
            dts: 0,
            data: Bytes::new(),
            payload_format: crate::media_frame::PayloadFormat::Amf0,
            key_frame: false,
            config_frame: false,
            received_at: Instant::now(),
        }
    }

    #[test]
    fn new_cache_empty() {
        let cache = GopCache::new();
        assert!(cache.get_all_frames().is_empty());
        assert!(cache.get_config_frames().is_empty());
        assert!(cache.get_latest_gop_frames().is_empty());
    }

    #[test]
    fn cache_video_config_frame() {
        let mut cache = GopCache::new();
        let cfg = make_video_frame(0, false, 7, 0, &[0x01, 0x02]);
        cache.cache_frame(&cfg);
        assert_eq!(cache.get_config_frames().len(), 1);
        assert_eq!(cache.get_all_frames().len(), 1);
    }

    #[test]
    fn cache_audio_config_frame() {
        let mut cache = GopCache::new();
        let cfg = make_audio_config();
        cache.cache_frame(&cfg);
        assert_eq!(cache.get_config_frames().len(), 1);
    }

    #[test]
    fn cache_metadata_frame() {
        let mut cache = GopCache::new();
        cache.cache_frame(&make_metadata());
        let all = cache.get_all_frames();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].frame_type, FrameType::Metadata);
    }

    #[test]
    fn regular_frame_not_in_config() {
        let mut cache = GopCache::new();
        let frame = make_video_frame(100, true, 7, 1, &[0x10, 0x20]);
        cache.cache_frame(&frame);
        assert!(cache.get_config_frames().is_empty());
        assert_eq!(cache.get_all_frames().len(), 1);
    }

    #[test]
    fn latest_gop_starts_from_keyframe() {
        let mut cache = GopCache::new();
        // Insert a key frame, then non-key frames.
        let key = make_video_frame(100, true, 7, 1, &[0x01]);
        let nonkey1 = make_video_frame(200, false, 7, 1, &[0x02]);
        let nonkey2 = make_video_frame(300, false, 7, 1, &[0x03]);
        cache.cache_frame(&key);
        cache.cache_frame(&nonkey1);
        cache.cache_frame(&nonkey2);
        let gop = cache.get_latest_gop_frames();
        assert_eq!(gop.len(), 3);
        assert!(gop[0].key_frame);
    }

    #[test]
    fn latest_gop_with_config_frames() {
        let mut cache = GopCache::new();
        cache.cache_frame(&make_video_config(ConfigType::H264));
        cache.cache_frame(&make_audio_config());
        let key = make_video_frame(100, true, 7, 1, &[0x01]);
        cache.cache_frame(&key);
        let gop = cache.get_latest_gop_frames();
        // Should include video config + audio config + key frame
        assert_eq!(gop.len(), 3);
    }

    #[test]
    fn config_frames_only_returns_config() {
        let mut cache = GopCache::new();
        cache.cache_frame(&make_video_config(ConfigType::H264));
        let frame = make_video_frame(100, true, 7, 1, &[0xAA]);
        cache.cache_frame(&frame);
        let cfgs = cache.get_config_frames();
        assert_eq!(cfgs.len(), 1);
        assert!(cfgs[0].config_frame || is_config_frame(&cfgs[0]));
    }

    #[test]
    fn clear_resets_cache() {
        let mut cache = GopCache::new();
        cache.cache_frame(&make_video_config(ConfigType::H264));
        cache.cache_frame(&make_video_frame(100, true, 7, 1, &[0x01]));
        assert!(!cache.get_all_frames().is_empty());
        cache.clear();
        assert!(cache.get_all_frames().is_empty());
        assert!(cache.get_config_frames().is_empty());
    }

    #[test]
    fn multiple_keyframes_keeps_latest_gop() {
        let mut cache = GopCache::new();
        for i in 0..10 {
            let key = make_video_frame(i * 100, true, 7, 1, &[i as u8]);
            cache.cache_frame(&key);
            for _ in 0..4 {
                let nk = make_video_frame(i * 100, false, 7, 1, &[i as u8]);
                cache.cache_frame(&nk);
            }
        }
        let gop = cache.get_latest_gop_frames();
        // Should contain the last keyframe + 4 non-keyframes = 5
        assert_eq!(gop.len(), 5);
        assert!(gop[0].key_frame);
    }

    enum ConfigType {
        H264,
    }

    fn make_video_config(ct: ConfigType) -> MediaFrame {
        match ct {
            ConfigType::H264 => {
                let data = vec![0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02];
                MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, Bytes::from(data), false)
            }
        }
    }
}
