//! Minimal XML parser for GB28181 MESSAGE bodies.
//!
//! GB28181 control messages (Catalog, DeviceInfo, Keepalive, Notify, ...)
//! carry a well-formed but tiny XML body. A lightweight DOM parser is
//! sufficient here and avoids pulling a full XML crate into the dependency
//! tree. It supports processing instructions, comments, attributes, nested
//! elements and the five standard XML character entities.

use std::collections::HashMap;

/// A parsed XML element.
#[derive(Debug, Clone, Default)]
pub struct XmlNode {
    pub name: String,
    /// Concatenated text content (direct children only).
    pub text: String,
    pub attrs: HashMap<String, String>,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    /// First direct child with the given name.
    pub fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.name == name)
    }

    /// All direct children with the given name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Value of the named attribute, if present.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(|s| s.as_str())
    }

    /// Text of the first direct child with the given name, if present.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }

    /// Text of the first descendant with the given name, if present.
    pub fn descendant_text(&self, name: &str) -> Option<&str> {
        self.descendants_named(name)
            .first()
            .map(|n| n.text.as_str())
    }

    /// All descendants (including direct children) with the given name.
    pub fn descendants_named(&self, name: &str) -> Vec<&XmlNode> {
        let mut out = Vec::new();
        self.collect_descendants(name, &mut out);
        out
    }

    fn collect_descendants<'a>(&'a self, name: &str, out: &mut Vec<&'a XmlNode>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.collect_descendants(name, out);
        }
    }
}

/// Parses a GB28181 XML document. Returns the root element.
pub fn parse_xml(input: &str) -> Option<XmlNode> {
    let bytes = input.as_bytes();
    let mut pos = 0;
    loop {
        skip_ws(bytes, &mut pos);
        if starts_with(bytes, pos, b"<?") {
            pos = skip_until(bytes, pos, b"?>")?;
            continue;
        }
        if starts_with(bytes, pos, b"<!--") {
            pos = skip_until(bytes, pos, b"-->")?;
            continue;
        }
        break;
    }
    if pos >= bytes.len() || bytes[pos] != b'<' {
        return None;
    }
    parse_element(bytes, pos).map(|(node, _)| node)
}

/// Parses an element starting at `bytes[pos] == '<'`.
fn parse_element(bytes: &[u8], pos: usize) -> Option<(XmlNode, usize)> {
    let mut p = pos + 1;
    if matches!(bytes.get(p), Some(b'?') | Some(b'!')) {
        return None;
    }
    let name_start = p;
    while p < bytes.len() && !matches!(bytes[p], b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n') {
        p += 1;
    }
    if p >= bytes.len() {
        return None;
    }
    let name = String::from_utf8_lossy(&bytes[name_start..p]).to_string();
    if name.is_empty() {
        return None;
    }

    let mut attrs = HashMap::new();
    let mut self_closing = false;
    loop {
        skip_ws(bytes, &mut p);
        if p >= bytes.len() {
            return None;
        }
        if bytes[p] == b'>' {
            p += 1;
            break;
        }
        if bytes[p] == b'/' && bytes.get(p + 1) == Some(&b'>') {
            self_closing = true;
            p += 2;
            break;
        }
        let a_start = p;
        while p < bytes.len()
            && !matches!(bytes[p], b'=' | b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
        {
            p += 1;
        }
        let a_name = String::from_utf8_lossy(&bytes[a_start..p]).to_string();
        if a_name.is_empty() {
            return None;
        }
        skip_ws(bytes, &mut p);
        if p >= bytes.len() || bytes[p] != b'=' {
            return None;
        }
        p += 1;
        skip_ws(bytes, &mut p);
        let quote = *bytes.get(p)?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        p += 1;
        let v_start = p;
        while p < bytes.len() && bytes[p] != quote {
            p += 1;
        }
        if p >= bytes.len() {
            return None;
        }
        let a_val = String::from_utf8_lossy(&bytes[v_start..p]).to_string();
        p += 1;
        attrs.insert(a_name, a_val);
    }

    if self_closing {
        return Some((
            XmlNode {
                name,
                attrs,
                ..Default::default()
            },
            p,
        ));
    }

    let mut children = Vec::new();
    let mut text = String::new();
    loop {
        let lt = find_byte(bytes, p, b'<')?;
        if lt > p {
            let chunk = String::from_utf8_lossy(&bytes[p..lt]);
            text.push_str(&unescape(&chunk));
        }
        if lt + 1 >= bytes.len() {
            return None;
        }
        if bytes[lt + 1] == b'/' {
            let mut q = lt + 2;
            while q < bytes.len() && bytes[q] != b'>' {
                q += 1;
            }
            if q >= bytes.len() {
                return None;
            }
            let close = String::from_utf8_lossy(&bytes[lt + 2..q])
                .trim()
                .to_string();
            if close != name {
                return None;
            }
            return Some((
                XmlNode {
                    name,
                    text,
                    attrs,
                    children,
                },
                q + 1,
            ));
        }
        if bytes[lt + 1] == b'?' {
            p = skip_until(bytes, lt + 1, b"?>")?;
        } else if bytes[lt + 1] == b'!' {
            p = skip_until(bytes, lt + 1, b"-->")?;
        } else {
            let (child, np) = parse_element(bytes, lt)?;
            children.push(child);
            p = np;
        }
    }
}

fn skip_ws(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\r' | b'\n') {
        *pos += 1;
    }
}

fn starts_with(bytes: &[u8], pos: usize, pat: &[u8]) -> bool {
    bytes.len() >= pos + pat.len() && &bytes[pos..pos + pat.len()] == pat
}

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == needle)
        .map(|i| from + i)
}

/// Returns the position immediately after the pattern, or `None`.
fn skip_until(bytes: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    let mut i = from;
    while i + pat.len() <= bytes.len() {
        if &bytes[i..i + pat.len()] == pat {
            return Some(i + pat.len());
        }
        i += 1;
    }
    None
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEPALIVE: &str = r#"<?xml version="1.0"?>
<Notify>
<CmdType>Keepalive</CmdType>
<SN>123</SN>
<DeviceID>34020000001320000001</DeviceID>
<Status>OK</Status>
</Notify>"#;

    #[test]
    fn parse_keepalive() {
        let root = parse_xml(KEEPALIVE).expect("parse xml");
        assert_eq!(root.name, "Notify");
        assert_eq!(root.text_of("CmdType"), Some("Keepalive"));
        assert_eq!(root.text_of("SN"), Some("123"));
        assert_eq!(root.text_of("DeviceID"), Some("34020000001320000001"));
        assert_eq!(root.text_of("Status"), Some("OK"));
    }

    #[test]
    fn parse_catalog_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Response>
<CmdType>Catalog</CmdType>
<SN>7</SN>
<DeviceID>34020000001320000001</DeviceID>
<SumNum>2</SumNum>
<DeviceList Num="2">
<Item>
<DeviceID>34020000001320000002</DeviceID>
<Name>Camera 1</Name>
<Manufacturer>Hikvision</Manufacturer>
<Model>DS-2CD</Model>
<Status>ON</Status>
<PTZType>1</PTZType>
<Longitude>120.1</Longitude>
<Latitude>30.2</Latitude>
</Item>
<Item>
<DeviceID>34020000001320000003</DeviceID>
<Name>Camera 2</Name>
<Status>OFF</Status>
</Item>
</DeviceList>
</Response>"#;
        let root = parse_xml(xml).expect("parse xml");
        assert_eq!(root.name, "Response");
        assert_eq!(root.text_of("CmdType"), Some("Catalog"));
        assert_eq!(root.text_of("SumNum"), Some("2"));
        let items = root.children_named("DeviceList").next().unwrap();
        assert_eq!(items.attr("Num"), Some("2"));
        let items: Vec<_> = items.children_named("Item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text_of("DeviceID"), Some("34020000001320000002"));
        assert_eq!(items[0].text_of("Name"), Some("Camera 1"));
        assert_eq!(items[1].text_of("Status"), Some("OFF"));
        assert_eq!(root.text_of("DeviceID"), Some("34020000001320000001"));
    }

    #[test]
    fn parse_entities_and_attrs() {
        let xml = r#"<Query a="1" b='two'>
<Name>A &amp; B &lt;tag&gt;</Name>
<SelfTag/>
</Query>"#;
        let root = parse_xml(xml).unwrap();
        assert_eq!(root.attr("a"), Some("1"));
        assert_eq!(root.attr("b"), Some("two"));
        assert_eq!(root.text_of("Name"), Some("A & B <tag>"));
        assert!(root.child("SelfTag").is_some());
    }

    #[test]
    fn malformed() {
        assert!(parse_xml("<Foo>").is_none());
        assert!(parse_xml("<Foo><Bar></Foo>").is_none());
        assert!(parse_xml("not xml").is_none());
    }
}
