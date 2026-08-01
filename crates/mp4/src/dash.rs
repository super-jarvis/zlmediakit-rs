//! DASH MPD (Media Presentation Description) manifest generator.
//!
//! Generates a dynamic MPD XML document describing the available
//! fMP4 representations for a live stream.

use zlmediakit_core::media_frame::CodecId;

/// Configuration for a DASH representation (one track).
#[derive(Debug, Clone)]
pub struct DashRepresentation {
    /// Unique ID for this representation.
    pub id: String,
    /// MIME type: "video/mp4" or "audio/mp4"
    pub mime_type: String,
    /// Codec string (e.g. "avc1.64000a").
    pub codecs: String,
    /// Width in pixels (0 for audio).
    pub width: u32,
    /// Height in pixels (0 for audio).
    pub height: u32,
    /// Segment duration in seconds.
    pub segment_duration_secs: u32,
    /// Timescale of this representation.
    pub timescale: u32,
    /// Bitrate hint in bits/sec.
    pub bandwidth: u32,
}

/// Builds a dynamic MPD XML string for live DASH streaming.
///
/// `representations` contains one entry per codec (video + optional audio).
/// `init_template` is the URL template for init segments (e.g. `$RepresentationID$/init.mp4`).
/// `media_template` is the URL template for media segments (e.g. `$RepresentationID$/seg-$Number$.m4s`).
pub fn build_mpd(
    representations: &[DashRepresentation],
    init_template: &str,
    media_template: &str,
    start_number: u64,
) -> String {
    let mut xml = String::new();
    xml.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    xml.push('\n');
    xml.push_str(r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" "#);
    xml.push_str(r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" "#);
    xml.push_str(r#"xsi:schemaLocation="urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd" "#);
    xml.push_str(r#"profiles="urn:mpeg:dash:profile:isoff-live:2011" "#);
    xml.push_str(r#"type="dynamic" "#);
    if let Some(repr) = representations.first() {
        xml.push_str(&format!(
            r#"minBufferTime="PT{}S" "#,
            repr.segment_duration_secs * 2
        ));
    }
    xml.push_str(r#"minimumUpdatePeriod="PT2S">"#);
    xml.push('\n');

    xml.push_str(r#"  <Period id="0" start="PT0S">"#);
    xml.push('\n');

    // AdaptationSet for video
    let video_reprs: Vec<&DashRepresentation> = representations
        .iter()
        .filter(|r| r.mime_type.starts_with("video"))
        .collect();
    if !video_reprs.is_empty() {
        xml.push_str(r#"    <AdaptationSet contentType="video" "#);
        xml.push_str(r#"segmentAlignment="true" bitstreamSwitching="true">"#);
        xml.push('\n');
        for r in &video_reprs {
            write_representation(&mut xml, r, init_template, media_template, start_number);
        }
        xml.push_str("    </AdaptationSet>\n");
    }

    // AdaptationSet for audio
    let audio_reprs: Vec<&DashRepresentation> = representations
        .iter()
        .filter(|r| r.mime_type.starts_with("audio"))
        .collect();
    if !audio_reprs.is_empty() {
        xml.push_str(r#"    <AdaptationSet contentType="audio" "#);
        xml.push_str(r#"segmentAlignment="true" bitstreamSwitching="true">"#);
        xml.push('\n');
        for r in &audio_reprs {
            write_representation(&mut xml, r, init_template, media_template, start_number);
        }
        xml.push_str("    </AdaptationSet>\n");
    }

    xml.push_str("  </Period>\n");
    xml.push_str("</MPD>\n");
    xml
}

fn write_representation(
    xml: &mut String,
    r: &DashRepresentation,
    init_template: &str,
    media_template: &str,
    start_number: u64,
) {
    xml.push_str(&format!(
        r#"      <Representation id="{}" mimeType="{}" codecs="{}" "#,
        r.id, r.mime_type, r.codecs
    ));
    if r.width > 0 {
        xml.push_str(&format!(r#"width="{}" height="{}" "#, r.width, r.height));
    }
    xml.push_str(&format!(r#"bandwidth="{}" "#, r.bandwidth));
    xml.push_str(r#"frameRate="30" "#);
    xml.push_str(">\n");

    xml.push_str(&format!(
        r#"        <SegmentTemplate timescale="{}" "#,
        r.timescale
    ));
    xml.push_str(&format!(
        r#"duration="{}" "#,
        r.segment_duration_secs * r.timescale
    ));
    xml.push_str(&format!(r#"startNumber="{}" "#, start_number));
    xml.push_str(&format!(r#"initialization="{}" "#, init_template));
    xml.push_str(&format!(r#"media="{}" />"#, media_template));
    xml.push('\n');

    xml.push_str("      </Representation>\n");
}

/// Returns a RFC 6381 codec string from a CodecId and config data.
pub fn codec_string(codec: CodecId, config: &[u8]) -> String {
    match codec {
        CodecId::H264 => {
            // Extract profile + level from AVCDecoderConfigurationRecord
            let avcc = if config.len() >= 5 && config[0] != 0x01 {
                &config[5..]
            } else if config.len() >= 3 && config[0] == 0x01 {
                config
            } else {
                return "avc1.64001f".to_string(); // default
            };
            if avcc.len() >= 4 {
                format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3])
            } else {
                "avc1.64001f".to_string()
            }
        }
        CodecId::H265 => {
            let hvcc = if config.len() >= 5 && config[0] != 0x01 {
                &config[5..]
            } else if config.len() >= 3 && config[0] == 0x01 {
                config
            } else {
                return "hev1.1.6.L93.B0".to_string();
            };
            if hvcc.len() >= 16 {
                let profile_space = (hvcc[1] >> 6) & 0x03;
                let _tier = (hvcc[1] >> 5) & 0x01;
                let profile = hvcc[1] & 0x1F;
                let compat = u32::from_be_bytes([0, hvcc[2], hvcc[3], hvcc[4]]);
                let level = hvcc[12];
                format!(
                    "hev1.{}.{}.L{}.B{}",
                    profile_space * 32 + profile,
                    compat,
                    level * 30,
                    0
                )
            } else {
                "hev1.1.6.L93.B0".to_string()
            }
        }
        CodecId::AAC => {
            let aac_cfg = if config.len() >= 2 && config[0] == 0xAF && config.len() > 2 {
                &config[2..]
            } else {
                config
            };
            if aac_cfg.len() >= 2 {
                let audio_object_type = (aac_cfg[0] >> 3) & 0x1F;
                let _freq_idx = ((aac_cfg[0] & 0x07) << 1) | ((aac_cfg[1] >> 7) & 0x01);
                format!("mp4a.40.{}", audio_object_type)
            } else {
                "mp4a.40.2".to_string()
            }
        }
        CodecId::VP8 => "vp8".to_string(),
        CodecId::VP9 => "vp09.00.10.08".to_string(),
        CodecId::AV1 => "av01.0.04M.08".to_string(),
        _ => "mp4a.40.2".to_string(),
    }
}

/// Maps a CodecId to an FLV/RTMP sound format code.
pub fn codec_to_sound_format(codec: CodecId) -> u8 {
    match codec {
        CodecId::AAC => 10,
        CodecId::Mp3 => 2,
        CodecId::G711A => 7,
        CodecId::G711U => 8,
        CodecId::Opus => 13,
        CodecId::L16 => 14,
        _ => 10,
    }
}

/// Maps a CodecId to an FLV/RTMP video codec ID.
pub fn codec_to_video_codec_id(codec: CodecId) -> u8 {
    match codec {
        CodecId::H264 => 7,
        CodecId::H265 => 12,
        CodecId::VP8 => 8,
        CodecId::VP9 => 9,
        CodecId::AV1 => 13, // enhanced RTMP
        _ => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_simple_mpd() {
        let reprs = vec![DashRepresentation {
            id: "v0".into(),
            mime_type: "video/mp4".into(),
            codecs: "avc1.64001f".into(),
            width: 640,
            height: 480,
            segment_duration_secs: 4,
            timescale: 90000,
            bandwidth: 2000000,
        }];

        let mpd = build_mpd(
            &reprs,
            "$RepresentationID$/init.mp4",
            "$RepresentationID$/seg-$Number$.m4s",
            1,
        );
        assert!(mpd.contains("<MPD"));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("Representation"));
        assert!(mpd.contains("avc1.64001f"));
    }

    #[test]
    fn codec_string_h264() {
        let cfg = vec![0x01, 0x64, 0x00, 0x0a]; // High profile, level 1.0
        assert_eq!(codec_string(CodecId::H264, &cfg), "avc1.64000A");
    }
}
