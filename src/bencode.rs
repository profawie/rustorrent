//! Bencode encoder/decoder with depth and size limits for untrusted input.

use crate::bail;
use crate::util::Result;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.get(key.as_bytes()),
            _ => None,
        }
    }
    pub fn int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<String> {
        self.bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }
    pub fn list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub fn s(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }
    pub fn b(b: &[u8]) -> Value {
        Value::Bytes(b.to_vec())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Value::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Value::List(l) => {
                out.push(b'l');
                for v in l {
                    v.encode_into(out);
                }
                out.push(b'e');
            }
            Value::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    out.extend_from_slice(k.len().to_string().as_bytes());
                    out.push(b':');
                    out.extend_from_slice(k);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

/// Build a dictionary value from (key, value) pairs.
pub fn dict(items: Vec<(&str, Value)>) -> Value {
    Value::Dict(
        items
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect(),
    )
}

const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    d: &'a [u8],
    pos: usize,
    info_span: Option<(usize, usize)>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8> {
        match self.d.get(self.pos) {
            Some(&c) => Ok(c),
            None => bail!("bencode: unexpected end"),
        }
    }

    fn parse(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            bail!("bencode: nested too deep");
        }
        match self.peek()? {
            b'i' => {
                self.pos += 1;
                let rest = &self.d[self.pos..];
                let end = match rest.iter().position(|&c| c == b'e') {
                    Some(e) => e,
                    None => bail!("bencode: bad int"),
                };
                let s = std::str::from_utf8(&rest[..end]).map_err(|_| "bencode: bad int")?;
                let v: i64 = s.parse().map_err(|_| "bencode: bad int")?;
                self.pos += end + 1;
                Ok(Value::Int(v))
            }
            b'l' => {
                self.pos += 1;
                let mut l = Vec::new();
                while self.peek()? != b'e' {
                    l.push(self.parse(depth + 1)?);
                }
                self.pos += 1;
                Ok(Value::List(l))
            }
            b'd' => {
                self.pos += 1;
                let mut m = BTreeMap::new();
                while self.peek()? != b'e' {
                    let k = self.parse_bytes()?;
                    let start = self.pos;
                    let v = self.parse(depth + 1)?;
                    if depth == 0 && k == b"info" {
                        self.info_span = Some((start, self.pos));
                    }
                    m.insert(k, v);
                }
                self.pos += 1;
                Ok(Value::Dict(m))
            }
            b'0'..=b'9' => Ok(Value::Bytes(self.parse_bytes()?)),
            _ => bail!("bencode: invalid data"),
        }
    }

    fn parse_bytes(&mut self) -> Result<Vec<u8>> {
        let rest = &self.d[self.pos..];
        let colon = match rest.iter().take(12).position(|&c| c == b':') {
            Some(c) => c,
            None => bail!("bencode: bad string"),
        };
        let len: usize = std::str::from_utf8(&rest[..colon])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("bencode: bad string length")?;
        let start = colon + 1;
        if len > rest.len() - start {
            bail!("bencode: string past end");
        }
        self.pos += start + len;
        Ok(rest[start..start + len].to_vec())
    }
}

/// Decode a value, ignoring any trailing bytes.
pub fn decode(d: &[u8]) -> Result<Value> {
    Ok(decode_prefix(d)?.0)
}

/// Decode a value and return how many bytes it occupied.
pub fn decode_prefix(d: &[u8]) -> Result<(Value, usize)> {
    let mut p = Parser {
        d,
        pos: 0,
        info_span: None,
    };
    let v = p.parse(0)?;
    Ok((v, p.pos))
}

/// Decode a .torrent file and return the byte span of the raw `info` dictionary.
pub fn decode_torrent(d: &[u8]) -> Result<(Value, Option<(usize, usize)>)> {
    let mut p = Parser {
        d,
        pos: 0,
        info_span: None,
    };
    let v = p.parse(0)?;
    Ok((v, p.info_span))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let v = dict(vec![
            ("a", Value::Int(-5)),
            ("b", Value::List(vec![Value::s("xy"), Value::Int(0)])),
            ("info", dict(vec![("n", Value::s("z"))])),
        ]);
        let e = v.encode();
        assert_eq!(e, b"d1:ai-5e1:bl2:xyi0ee4:infod1:n1:zee".to_vec());
        let (d, span) = decode_torrent(&e).unwrap();
        assert_eq!(d, v);
        let (s, t) = span.unwrap();
        assert_eq!(&e[s..t], b"d1:n1:ze");
        assert!(decode(b"l").is_err());
        assert!(decode(b"5:ab").is_err());
        assert!(decode(&[b'l'; 200]).is_err());
        assert!(decode(b"99999999999999999999:").is_err());
    }
}
