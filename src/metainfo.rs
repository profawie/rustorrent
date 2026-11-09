//! .torrent metainfo parsing (BEP 3) with path sanitising for Windows.

use crate::bail;
use crate::bencode::{self, Value};
use crate::util::{Result, hex, sha1};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Path relative to the download directory (includes the torrent root folder).
    pub path: PathBuf,
    pub length: u64,
    /// Offset of the file within the torrent's concatenated data.
    pub offset: u64,
    /// BEP 47 padding file: never written to disk.
    pub pad: bool,
}

#[derive(Debug)]
pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    pub hashes: Vec<[u8; 20]>,
    pub files: Vec<FileEntry>,
    pub total_length: u64,
    /// Truncated SHA-256 info hash for hybrid v1/v2 torrents (BEP 52).
    pub info_hash_v2: Option<[u8; 20]>,
    pub info_bytes: Vec<u8>,
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Make one path component safe on Windows (and everywhere else).
pub fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    let out = out.trim_start().to_string();
    if out.is_empty() {
        return "_".into();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let mut out = if RESERVED.contains(&stem.as_str()) {
        format!("_{}", out)
    } else {
        out
    };
    if out.chars().count() > 180 {
        out = out.chars().take(180).collect();
    }
    out
}

impl Metainfo {
    pub fn from_info(info_bytes: Vec<u8>) -> Result<Metainfo> {
        let info = bencode::decode(&info_bytes)?;
        if info.dict().is_none() {
            bail!("info is not a dictionary");
        }
        let info_hash = sha1(&info_bytes);
        let name_raw = info
            .get("name.utf-8")
            .or_else(|| info.get("name"))
            .and_then(|v| v.str())
            .unwrap_or_default();
        let name = if name_raw.trim().is_empty() {
            hex(&info_hash)
        } else {
            sanitize(&name_raw)
        };
        let piece_length = match info.get("piece length").and_then(Value::int) {
            Some(p) if p > 0 && p <= 1 << 28 => p as u64,
            _ => bail!("invalid piece length"),
        };
        let pieces = info
            .get("pieces")
            .and_then(Value::bytes)
            .ok_or("missing pieces")?;
        if pieces.len() % 20 != 0 {
            bail!("invalid pieces field");
        }
        let hashes: Vec<[u8; 20]> = pieces.chunks(20).map(|c| c.try_into().unwrap()).collect();

        let mut files = Vec::new();
        let mut offset = 0u64;
        if let Some(list) = info.get("files").and_then(Value::list) {
            if list.len() > 200_000 {
                bail!("too many files");
            }
            for f in list {
                let length = match f.get("length").and_then(Value::int) {
                    Some(l) if l >= 0 => l as u64,
                    _ => bail!("invalid file length"),
                };
                let comps = f
                    .get("path.utf-8")
                    .or_else(|| f.get("path"))
                    .and_then(Value::list)
                    .ok_or("invalid file path")?;
                let mut path = PathBuf::from(&name);
                for c in comps {
                    path.push(sanitize(&c.str().ok_or("invalid file path")?));
                }
                if comps.is_empty() {
                    path.push("_");
                }
                let pad = f
                    .get("attr")
                    .and_then(|a| a.str())
                    .is_some_and(|a| a.contains('p'));
                files.push(FileEntry {
                    path,
                    length,
                    offset,
                    pad,
                });
                offset = offset.checked_add(length).ok_or("torrent too large")?;
            }
        } else {
            let length = match info.get("length").and_then(Value::int) {
                Some(l) if l >= 0 => l as u64,
                _ => bail!("invalid length"),
            };
            files.push(FileEntry {
                path: PathBuf::from(&name),
                length,
                offset: 0,
                pad: false,
            });
            offset = length;
        }
        let total_length = offset;
        let expected = total_length.div_ceil(piece_length);
        if expected != hashes.len() as u64 || hashes.is_empty() {
            bail!("piece count does not match total size");
        }
        let info_hash_v2 = (info.get("meta version").and_then(Value::int) == Some(2))
            .then(|| crate::util::sha256(&info_bytes)[..20].try_into().unwrap());
        Ok(Metainfo {
            info_hash,
            name,
            piece_length,
            hashes,
            files,
            total_length,
            info_hash_v2,
            info_bytes,
        })
    }

    /// Parse a full .torrent file; returns the metainfo and tracker URLs.
    pub fn from_torrent(data: &[u8]) -> Result<(Metainfo, Vec<String>)> {
        let (root, span) = bencode::decode_torrent(data)?;
        let (s, e) = span.ok_or("torrent has no info dictionary")?;
        let meta = Metainfo::from_info(data[s..e].to_vec())?;
        let mut trackers: Vec<String> = Vec::new();
        if let Some(tiers) = root.get("announce-list").and_then(Value::list) {
            for tier in tiers {
                for t in tier.list().unwrap_or(&[]) {
                    if let Some(u) = t.str() {
                        if !trackers.contains(&u) {
                            trackers.push(u);
                        }
                    }
                }
            }
        }
        if let Some(u) = root.get("announce").and_then(Value::str) {
            if !trackers.contains(&u) {
                trackers.insert(0, u);
            }
        }
        trackers.retain(|t| !t.trim().is_empty());
        Ok((meta, trackers))
    }

    pub fn num_pieces(&self) -> usize {
        self.hashes.len()
    }

    pub fn piece_len(&self, i: usize) -> u64 {
        if i + 1 == self.hashes.len() {
            self.total_length - self.piece_length * (self.hashes.len() as u64 - 1)
        } else {
            self.piece_length
        }
    }
}

/// Build a .torrent for a single file (used by tests and the CLI tool).
#[cfg(any(test, not(windows)))]
pub fn create_single(
    path: &std::path::Path,
    piece_length: u64,
    tracker: Option<&str>,
) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    let mut pieces = Vec::new();
    for c in data.chunks(piece_length as usize) {
        pieces.extend_from_slice(&sha1(c));
    }
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let info = bencode::dict(vec![
        ("length", Value::Int(data.len() as i64)),
        ("name", Value::s(&name)),
        ("piece length", Value::Int(piece_length as i64)),
        ("pieces", Value::Bytes(pieces)),
    ]);
    let mut top = vec![("info", info)];
    if let Some(t) = tracker {
        top.push(("announce", Value::s(t)));
    }
    Ok(bencode::dict(top).encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("a/b:c"), "a_b_c");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("x. "), "x");
    }
    #[test]
    fn parse_multi() {
        let info = bencode::dict(vec![
            (
                "files",
                Value::List(vec![
                    bencode::dict(vec![
                        ("length", Value::Int(5)),
                        ("path", Value::List(vec![Value::s("a"), Value::s("..")])),
                    ]),
                    bencode::dict(vec![
                        ("length", Value::Int(7)),
                        ("path", Value::List(vec![Value::s("b.txt")])),
                    ]),
                ]),
            ),
            ("name", Value::s("root")),
            ("piece length", Value::Int(8)),
            ("pieces", Value::Bytes(vec![0; 40])),
        ]);
        let t = bencode::dict(vec![("announce", Value::s("http://x/a")), ("info", info)]).encode();
        let (m, tr) = Metainfo::from_torrent(&t).unwrap();
        assert_eq!(tr, vec!["http://x/a".to_string()]);
        assert_eq!(m.total_length, 12);
        assert_eq!(m.files[0].path, PathBuf::from("root").join("a").join("_"));
        assert_eq!(m.files[1].offset, 5);
        assert_eq!(m.piece_len(1), 4);
    }
}
