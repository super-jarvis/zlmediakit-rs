use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RtspRequest {
    pub method: String,
    pub uri: String,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub version: String,
    pub status_code: u16,
    pub reason: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl RtspResponse {
    pub fn new(status_code: u16, reason: &str) -> Self {
        let mut headers = HashMap::new();
        headers.insert("CSeq".to_string(), "0".to_string());
        Self {
            version: "RTSP/1.0".to_string(),
            status_code,
            reason: reason.to_string(),
            headers,
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut response = format!("{} {} {}\r\n", self.version, self.status_code, self.reason);
        for (key, value) in &self.headers {
            response.push_str(&format!("{}: {}\r\n", key, value));
        }
        if !self.body.is_empty() {
            response.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        response.push_str("\r\n");
        let mut bytes = response.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

pub struct RtspParser;

impl RtspParser {
    pub fn parse_request(data: &[u8]) -> Option<(RtspRequest, usize)> {
        let data_str = std::str::from_utf8(data).ok()?;
        let header_end = data_str.find("\r\n\r\n")?;
        let header_str = &data_str[..header_end];
        let body_start = header_end + 4;

        let mut lines = header_str.lines();
        let request_line = lines.next()?;
        let parts: Vec<&str> = request_line.splitn(3, ' ').collect();
        if parts.len() < 3 {
            return None;
        }

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        let content_length = headers
            .get("Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let total_len = body_start + content_length;
        if data.len() < total_len {
            return None;
        }

        let body = data[body_start..total_len].to_vec();

        Some((
            RtspRequest {
                method: parts[0].to_string(),
                uri: parts[1].to_string(),
                version: parts[2].to_string(),
                headers,
                body,
            },
            total_len,
        ))
    }

    pub fn parse_response(data: &[u8]) -> Option<(RtspResponse, usize)> {
        let data_str = std::str::from_utf8(data).ok()?;
        let header_end = data_str.find("\r\n\r\n")?;
        let header_str = &data_str[..header_end];
        let body_start = header_end + 4;

        let mut lines = header_str.lines();
        let status_line = lines.next()?;
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        if parts.len() < 3 {
            return None;
        }

        let status_code = parts[1].parse::<u16>().ok()?;

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        let content_length = headers
            .get("Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let total_len = body_start + content_length;
        if data.len() < total_len {
            return None;
        }

        let body = data[body_start..total_len].to_vec();

        Some((
            RtspResponse {
                version: parts[0].to_string(),
                status_code,
                reason: parts[2].to_string(),
                headers,
                body,
            },
            total_len,
        ))
    }

    pub fn parse_sdp(data: &[u8]) -> HashMap<String, String> {
        let mut result = HashMap::new();
        if let Ok(s) = std::str::from_utf8(data) {
            for line in s.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    result.insert(key.to_string(), value.to_string());
                }
            }
        }
        result
    }
}
