//! Magnet URI parsing and generation.

use crate::bail;
use crate::util::{Result, hex, unhex, url_decode, url_encode};

pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: Option<String>,
    pub trackers: Vec<String>,
}

fn base32(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u64, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buf = (buf << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn parse(uri: &str) -> Result<Magnet> {
    let uri = uri.trim();
    let q = match uri.strip_prefix("magnet:?") {
        Some(q) => q,
        None => bail!("not a magnet link"),
    };
    let mut ih = None;
    let mut name = None;
    let mut trackers = Vec::new();
    for part in q.split('&') {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let val = String::from_utf8_lossy(&url_decode(v)).into_owned();
        match k {
            "xt" => {
                if let Some(h) = val.strip_prefix("urn:btih:") {
                    let bytes = if h.len() == 40 {
                        unhex(h)
                    } else if h.len() == 32 {
                        base32(h)
                    } else {
                        None
                    };
                    if let Some(b) = bytes {
                        if b.len() == 20 && ih.is_none() {
                            let mut a = [0u8; 20];
                            a.copy_from_slice(&b);
                            ih = Some(a);
                        }
                    }
                }
            }
            "dn" => name = Some(val),
            "tr" if !trackers.contains(&val) => trackers.push(val),
            _ => {}
        }
    }
    match ih {
        Some(info_hash) => Ok(Magnet {
            info_hash,
            name,
            trackers,
        }),
        None => bail!("magnet link has no BitTorrent info hash"),
    }
}

pub fn build(info_hash: &[u8; 20], name: &str, trackers: &[String]) -> String {
    let mut s = format!("magnet:?xt=urn:btih:{}", hex(info_hash));
    if !name.is_empty() {
        s.push_str("&dn=");
        s.push_str(&url_encode(name.as_bytes()));
    }
    for t in trackers {
        s.push_str("&tr=");
        s.push_str(&url_encode(t.as_bytes()));
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_magnet() {
        let m = super::parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=Foo+Bar&tr=udp%3A%2F%2Ft.example%3A80").unwrap();
        assert_eq!(m.info_hash[0], 0xc1);
        assert_eq!(m.name.as_deref(), Some("Foo Bar"));
        assert_eq!(m.trackers, vec!["udp://t.example:80".to_string()]);
        let b = super::parse("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK").unwrap();
        assert_eq!(b.info_hash, m.info_hash);
        let s = super::build(&m.info_hash, "Foo Bar", &m.trackers);
        assert_eq!(super::parse(&s).unwrap().trackers, m.trackers);
    }
}
