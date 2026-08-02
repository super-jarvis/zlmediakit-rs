use anyhow::{anyhow, bail, Result};

pub(crate) fn parse_rtmp_url(url: &str) -> Result<(String, u16, String, String, bool)> {
    let (rest, use_tls) = if let Some(rest) = url.strip_prefix("rtmps://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("rtmp://") {
        (rest, false)
    } else {
        bail!("not an RTMP(S) URL: {url}");
    };
    let default_port = if use_tls { 443 } else { 1935 };
    let (authority, path) = rest
        .find('/')
        .map_or((rest, ""), |index| (&rest[..index], &rest[index + 1..]));
    if authority.is_empty() {
        bail!("RTMP URL is missing a host");
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed IPv6 RTMP host"))?;
        let suffix = &bracketed[end + 1..];
        (
            bracketed[..end].to_string(),
            suffix
                .strip_prefix(':')
                .map(str::parse)
                .transpose()?
                .unwrap_or(default_port),
        )
    } else if authority.matches(':').count() == 1 {
        let (host, port) = authority.rsplit_once(':').expect("count checked");
        (host.to_string(), port.parse()?)
    } else {
        (authority.to_string(), default_port)
    };
    let (app, stream) = path
        .split_once('/')
        .map(|(app, stream)| (app.to_string(), stream.to_string()))
        .unwrap_or_else(|| (path.to_string(), String::new()));
    Ok((host, port, app, stream, use_tls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv6_rtmp_and_rtmps_urls() {
        assert_eq!(
            parse_rtmp_url("rtmp://[::1]:1936/live/camera").unwrap(),
            (
                "::1".to_string(),
                1936,
                "live".to_string(),
                "camera".to_string(),
                false,
            )
        );
        assert_eq!(
            parse_rtmp_url("rtmps://[2001:db8::1]/secure/stream").unwrap(),
            (
                "2001:db8::1".to_string(),
                443,
                "secure".to_string(),
                "stream".to_string(),
                true,
            )
        );
    }
}
