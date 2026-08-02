//! Shared MP4 video-on-demand support for the protocol frontends.

use crate::{DemuxedTrack, Mp4Demuxer, TrackKind};
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;
use zlmediakit_core::media_frame::{
    AudioInfo, FrameType, MediaFrame, MediaInfo, TrackInfo, VideoInfo,
};

/// The application name used by RTMP/RTSP to address recorded files.
pub const VOD_APP: &str = "record";

/// A parsed MP4 archive that can be replayed independently by many clients.
#[derive(Debug, Clone)]
pub struct VodAsset {
    tracks: Vec<DemuxedTrack>,
    frames: Vec<MediaFrame>,
    duration_ms: u64,
}

/// A finite, timestamp-rebased view of a VOD asset.
#[derive(Debug, Clone)]
pub struct VodPlayback {
    pub frames: Vec<MediaFrame>,
    /// Actual random-access point selected in the original file timeline.
    pub start_ms: u64,
    pub duration_ms: u64,
}

impl VodAsset {
    pub fn from_demuxer(demuxer: &Mp4Demuxer) -> Result<Self> {
        let tracks = demuxer.tracks();
        let frames = demuxer.interleaved_media_frames();
        if tracks.is_empty() || frames.iter().all(|frame| frame.config_frame) {
            bail!("mp4 contains no playable media samples");
        }
        let duration_ms = tracks
            .iter()
            .filter(|track| track.timescale != 0)
            .map(|track| track.duration.saturating_mul(1000) / u64::from(track.timescale))
            .max()
            .unwrap_or_else(|| {
                frames
                    .iter()
                    .filter(|frame| !frame.config_frame)
                    .map(|frame| frame.pts.max(frame.dts))
                    .max()
                    .unwrap_or(0)
            });
        Ok(Self {
            tracks,
            frames,
            duration_ms,
        })
    }

    pub fn tracks(&self) -> &[DemuxedTrack] {
        &self.tracks
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub fn media_info(&self) -> MediaInfo {
        let tracks = self
            .tracks
            .iter()
            .filter_map(|track| match track.handler {
                TrackKind::Video => Some(TrackInfo::Video(VideoInfo {
                    codec: track.codec,
                    width: track.width,
                    height: track.height,
                    fps: track.fps,
                    key_frame: true,
                })),
                TrackKind::Audio => Some(TrackInfo::Audio(AudioInfo {
                    codec: track.codec,
                    sample_rate: track.sample_rate.max(1),
                    // The current demuxer does not expose the channel count yet.
                    channels: 2,
                    bits_per_sample: 16,
                })),
                TrackKind::Other => None,
            })
            .collect();
        MediaInfo {
            tracks,
            duration: Some(self.duration_ms as f64 / 1000.0),
            metadata: Some("mp4-vod".to_string()),
        }
    }

    /// Selects the nearest preceding video keyframe and rebases timestamps to
    /// zero. Decoder configuration frames are always emitted first.
    pub fn playback(&self, requested_start_ms: u64) -> VodPlayback {
        let first_media = self
            .frames
            .iter()
            .find(|frame| !frame.config_frame)
            .map(frame_time_ms)
            .unwrap_or(0);
        let first_video_key = self
            .frames
            .iter()
            .find(|frame| {
                !frame.config_frame && frame.frame_type == FrameType::Video && frame.key_frame
            })
            .map(frame_time_ms);
        let selected_key = self
            .frames
            .iter()
            .filter(|frame| {
                !frame.config_frame
                    && frame.frame_type == FrameType::Video
                    && frame.key_frame
                    && frame_time_ms(frame) <= requested_start_ms
            })
            .map(frame_time_ms)
            .max();
        let start_ms = selected_key.or(first_video_key).unwrap_or_else(|| {
            self.frames
                .iter()
                .filter(|frame| !frame.config_frame && frame_time_ms(frame) <= requested_start_ms)
                .map(frame_time_ms)
                .max()
                .unwrap_or(first_media)
        });

        let mut frames = Vec::new();
        frames.extend(
            self.frames
                .iter()
                .filter(|frame| frame.config_frame)
                .cloned(),
        );
        frames.extend(
            self.frames
                .iter()
                .filter(|frame| !frame.config_frame && frame_time_ms(frame) >= start_ms)
                .cloned()
                .map(|mut frame| {
                    frame.timestamp =
                        u32::try_from(u64::from(frame.timestamp).saturating_sub(start_ms))
                            .unwrap_or(u32::MAX);
                    frame.pts = frame.pts.saturating_sub(start_ms);
                    frame.dts = frame.dts.saturating_sub(start_ms);
                    frame
                }),
        );

        VodPlayback {
            frames,
            start_ms,
            duration_ms: self.duration_ms.saturating_sub(start_ms),
        }
    }
}

fn frame_time_ms(frame: &MediaFrame) -> u64 {
    frame.dts.min(frame.pts).min(u64::from(frame.timestamp))
}

/// Resolves and opens MP4 archives beneath one recording root.
#[derive(Debug, Clone)]
pub struct Mp4VodLibrary {
    root: PathBuf,
}

impl Mp4VodLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, app: &str, stream: &str) -> Option<PathBuf> {
        if app != VOD_APP {
            return None;
        }
        let decoded = percent_decode(stream)?;
        if decoded.is_empty()
            || decoded.contains('\\')
            || decoded.contains(':')
            || !decoded.to_ascii_lowercase().ends_with(".mp4")
        {
            return None;
        }
        let relative = Path::new(decoded.trim_start_matches('/'));
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        Some(self.root.join(relative))
    }

    pub async fn open(&self, app: &str, stream: &str) -> Result<Option<VodAsset>> {
        let Some(path) = self.resolve(app, stream) else {
            return Ok(None);
        };
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        }
        let demuxer = Mp4Demuxer::open(&path).await?;
        Ok(Some(VodAsset::from_demuxer(&demuxer)?))
    }
}

/// Real-time pacing helper shared by RTMP, RTSP and HTTP-FLV VOD output.
pub struct VodPacer {
    origin: Instant,
}

impl VodPacer {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub async fn wait(&self, timestamp_ms: u32) {
        tokio::time::sleep_until(self.origin + Duration::from_millis(u64::from(timestamp_ms)))
            .await;
    }
}

impl Default for VodPacer {
    fn default() -> Self {
        Self::new()
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use zlmediakit_core::media_frame::{CodecId, PayloadFormat};

    fn video(timestamp: u32, key: bool) -> MediaFrame {
        MediaFrame::new_video(
            1,
            CodecId::H264,
            timestamp,
            u64::from(timestamp),
            u64::from(timestamp),
            Bytes::from_static(&[0x17, 1, 0, 0, 0]),
            key,
        )
        .with_payload_format(PayloadFormat::Flv)
    }

    fn asset() -> VodAsset {
        let mut config = video(0, true);
        config.config_frame = true;
        VodAsset {
            tracks: vec![DemuxedTrack {
                track_id: 1,
                handler: TrackKind::Video,
                codec: CodecId::H264,
                timescale: 1000,
                duration: 4000,
                config: Bytes::new(),
                sample_count: 4,
                width: 1920,
                height: 1080,
                fps: 25.0,
                sample_rate: 0,
            }],
            frames: vec![
                config,
                video(0, true),
                video(1000, false),
                video(2000, true),
                video(3000, false),
            ],
            duration_ms: 4000,
        }
    }

    #[test]
    fn seek_uses_preceding_keyframe_and_rebases_timestamps() {
        let playback = asset().playback(2500);
        assert_eq!(playback.start_ms, 2000);
        assert_eq!(playback.duration_ms, 2000);
        assert!(playback.frames[0].config_frame);
        assert_eq!(playback.frames[1].timestamp, 0);
        assert!(playback.frames[1].key_frame);
        assert_eq!(playback.frames[2].timestamp, 1000);
    }

    #[test]
    fn resolver_rejects_path_traversal_and_decoded_separators() {
        let library = Mp4VodLibrary::new("record");
        assert_eq!(
            library.resolve(VOD_APP, "live/camera/file.mp4"),
            Some(PathBuf::from("record/live/camera/file.mp4"))
        );
        assert!(library.resolve(VOD_APP, "../secret.mp4").is_none());
        assert!(library.resolve(VOD_APP, "%2e%2e/secret.mp4").is_none());
        assert!(library.resolve(VOD_APP, "live%5csecret.mp4").is_none());
        assert!(library.resolve("live", "camera.mp4").is_none());
    }
}
