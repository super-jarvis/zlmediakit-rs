use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodecId {
    H264,
    H265,
    AAC,
    G711A,
    G711U,
    Opus,
    Mp3,
    L16,
    VP8,
    VP9,
    AV1,
    JPEG,
    MP2V,
    MP2A,
    Unknown(u32),
}

impl CodecId {
    pub fn is_video(&self) -> bool {
        matches!(
            self,
            CodecId::H264
                | CodecId::H265
                | CodecId::VP8
                | CodecId::VP9
                | CodecId::AV1
                | CodecId::JPEG
                | CodecId::MP2V
        )
    }

    pub fn is_audio(&self) -> bool {
        matches!(
            self,
            CodecId::AAC
                | CodecId::G711A
                | CodecId::G711U
                | CodecId::Opus
                | CodecId::Mp3
                | CodecId::L16
                | CodecId::MP2A
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameType {
    Video,
    Audio,
    Metadata,
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub codec: CodecId,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub key_frame: bool,
}

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub codec: CodecId,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
}

#[derive(Debug, Clone)]
pub enum TrackInfo {
    Video(VideoInfo),
    Audio(AudioInfo),
}

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub tracks: Vec<TrackInfo>,
    pub duration: Option<f64>,
    pub metadata: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub track_id: u32,
    pub frame_type: FrameType,
    pub codec: CodecId,
    pub timestamp: u32,
    pub pts: u64,
    pub dts: u64,
    pub data: Bytes,
    pub key_frame: bool,
    pub config_frame: bool,
    pub received_at: Instant,
}

impl MediaFrame {
    pub fn new_video(
        track_id: u32,
        codec: CodecId,
        timestamp: u32,
        pts: u64,
        dts: u64,
        data: Bytes,
        key_frame: bool,
    ) -> Self {
        Self {
            track_id,
            frame_type: FrameType::Video,
            codec,
            timestamp,
            pts,
            dts,
            data,
            key_frame,
            config_frame: false,
            received_at: Instant::now(),
        }
    }

    pub fn new_audio(
        track_id: u32,
        codec: CodecId,
        timestamp: u32,
        pts: u64,
        dts: u64,
        data: Bytes,
    ) -> Self {
        Self {
            track_id,
            frame_type: FrameType::Audio,
            codec,
            timestamp,
            pts,
            dts,
            data,
            key_frame: false,
            config_frame: false,
            received_at: Instant::now(),
        }
    }

    pub fn new_metadata(track_id: u32, timestamp: u32, data: Bytes) -> Self {
        Self {
            track_id,
            frame_type: FrameType::Metadata,
            codec: CodecId::Unknown(0),
            timestamp,
            pts: timestamp as u64,
            dts: timestamp as u64,
            data,
            key_frame: false,
            config_frame: true,
            received_at: Instant::now(),
        }
    }
}
