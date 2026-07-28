use crate::media_frame::CodecId;

pub fn parse_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim_start_matches("rtmp://")
        .trim_start_matches("rtsp://")
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    
    let parts: Vec<&str> = url.splitn(2, '/').collect();
    if parts.len() < 2 {
        return None;
    }
    
    let path_parts: Vec<&str> = parts[1].splitn(2, '/').collect();
    if path_parts.len() < 2 {
        return None;
    }
    
    Some((
        "__defaultVhost__".to_string(),
        path_parts[0].to_string(),
        path_parts[1].to_string(),
    ))
}

pub fn codec_id_to_rtmp_audio_tag(codec: CodecId) -> u8 {
    match codec {
        CodecId::AAC => 10,
        CodecId::Mp3 => 2,
        CodecId::G711A => 7,
        CodecId::G711U => 8,
        CodecId::Opus => 13,
        _ => 0,
    }
}

pub fn codec_id_to_rtsp_codec_name(codec: CodecId) -> &'static str {
    match codec {
        CodecId::H264 => "H264",
        CodecId::H265 => "H265",
        CodecId::AAC => "MPEG4-GENERIC",
        CodecId::G711A => "PCMA",
        CodecId::G711U => "PCMU",
        CodecId::Opus => "OPUS",
        CodecId::Mp3 => "MPA",
        _ => "UNKNOWN",
    }
}

pub fn random_string(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect()
}
