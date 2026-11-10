//! Tracker announces: HTTP(S) (BEP 3/23) and UDP (BEP 15).

use crate::bail;
use crate::bencode::{self, Value};
use crate::util::{Result, rand_u64, url_encode};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub enum Event {
    None,
    Started,
    Completed,
}

pub struct Announce<'a> {
    pub info_hash: &'a [u8; 20],
    pub peer_id: &'a [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

pub struct Response {
    pub interval: u64,
    pub peers: Vec<SocketAddr>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
}

const TIMEOUT: Duration = Duration::from_secs(15);

pub fn announce(url: &str, a: &Announce) -> Result<Response> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("udp://") {
        udp_announce(url, a)
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        http_announce(url, a)
    } else {
        bail!("unsupported tracker scheme")
    }
}

struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url> {
    let (scheme, rest) = url.split_once("://").ok_or("bad url")?;
    let scheme = scheme.to_ascii_lowercase();
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => 0,
    };
    let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
        let (h, p) = h.split_once(']').ok_or("bad url")?;
        (
            h.to_string(),
            p.strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port),
        )
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "bad port")?),
            None => (hostport.to_string(), default_port),
        }
    };
    if host.is_empty() || port == 0 {
        bail!("bad url");
    }
    Ok(Url {
        https: scheme == "https",
        host,
        port,
        path,
    })
}

fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    if addrs.is_empty() {
        bail!("could not resolve {}", host);
    }
    Ok(addrs)
}

fn http_announce(url: &str, a: &Announce) -> Result<Response> {
    let u = parse_url(url)?;
    let mut path = u.path.clone();
    path.push(if path.contains('?') { '&' } else { '?' });
    path.push_str(&format!(
        "info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=200&key={:08x}",
        url_encode(a.info_hash),
        url_encode(a.peer_id),
        a.port,
        a.uploaded,
        a.downloaded,
        a.left,
        rand_u64() as u32
    ));
    match a.event {
        Event::Started => path.push_str("&event=started"),
        Event::Completed => path.push_str("&event=completed"),
        Event::None => {}
    }
    let body = if u.https {
        https_get(&u, &path)?
    } else {
        http_get(&u, &path)?
    };
    parse_http_response(&body)
}

fn http_get(u: &Url, path: &str) -> Result<Vec<u8>> {
    let mut last_err = crate::util::Error("connect failed".into());
    for addr in resolve(&u.host, u.port)? {
        let mut s = match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                last_err = e.into();
                continue;
            }
        };
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        let req = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: rustorrent/{}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
            path,
            u.host,
            env!("CARGO_PKG_VERSION")
        );
        s.write_all(req.as_bytes())?;
        let mut resp = Vec::new();
        s.take(4 << 20).read_to_end(&mut resp)?;
        let hdr_end = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("bad http response")?;
        let head = String::from_utf8_lossy(&resp[..hdr_end]).to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).unwrap_or("");
        if !status.starts_with('2') {
            bail!("tracker http status {}", status);
        }
        let body = resp[hdr_end + 4..].to_vec();
        if head.contains("transfer-encoding: chunked") {
            return Ok(dechunk(&body));
        }
        return Ok(body);
    }
    Err(last_err)
}

pub fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let line = String::from_utf8_lossy(&b[..i]);
        let size =
            usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        b = &b[i + 2..];
        if size == 0 || size > b.len() {
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[(size + 2).min(b.len())..];
    }
    out
}

#[cfg(windows)]
fn https_get(u: &Url, path: &str) -> Result<Vec<u8>> {
    use windows_sys::Win32::Networking::WinHttp::*;
    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    struct H(*mut core::ffi::c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }
    unsafe {
        let agent = w("rustorrent");
        let sess = H(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if sess.0.is_null() {
            bail!("WinHttpOpen failed");
        }
        let t = TIMEOUT.as_millis() as i32;
        WinHttpSetTimeouts(sess.0, t, t, t, t);
        let host = w(&u.host);
        let conn = H(WinHttpConnect(sess.0, host.as_ptr(), u.port, 0));
        if conn.0.is_null() {
            bail!("https connect failed");
        }
        let verb = w("GET");
        let p = w(path);
        let req = H(WinHttpOpenRequest(
            conn.0,
            verb.as_ptr(),
            p.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            bail!("https request failed");
        }
        if WinHttpSendRequest(req.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
            bail!("https send failed");
        }
        if WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
            bail!("https receive failed");
        }
        let mut status: u32 = 0;
        let mut sz = 4u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            &mut status as *mut u32 as *mut _,
            &mut sz,
            std::ptr::null_mut(),
        );
        if !(200..300).contains(&status) {
            bail!("tracker http status {}", status);
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16384];
        loop {
            let mut n = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut n) == 0 {
                bail!("https read failed");
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > 4 << 20 {
                bail!("tracker response too large");
            }
        }
        Ok(out)
    }
}

#[cfg(not(windows))]
fn https_get(_u: &Url, _path: &str) -> Result<Vec<u8>> {
    bail!("https trackers are only supported on Windows")
}

pub fn parse_compact_v4(b: &[u8]) -> Vec<SocketAddr> {
    b.as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(c[0], c[1], c[2], c[3])),
                u16::from_be_bytes([c[4], c[5]]),
            )
        })
        .filter(|a| a.port() != 0)
        .collect()
}

pub fn parse_compact_v6(b: &[u8]) -> Vec<SocketAddr> {
    b.as_chunks::<18>()
        .0
        .iter()
        .map(|c| {
            let ip: [u8; 16] = c[..16].try_into().unwrap();
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(ip)),
                u16::from_be_bytes([c[16], c[17]]),
            )
        })
        .filter(|a| a.port() != 0)
        .collect()
}

fn parse_http_response(body: &[u8]) -> Result<Response> {
    let v = bencode::decode(body)?;
    if let Some(f) = v.get("failure reason").and_then(Value::str) {
        bail!("tracker: {}", f);
    }
    let interval = v
        .get("interval")
        .and_then(Value::int)
        .unwrap_or(1800)
        .max(0) as u64;
    let mut peers = Vec::new();
    match v.get("peers") {
        Some(Value::Bytes(b)) => peers.extend(parse_compact_v4(b)),
        Some(Value::List(l)) => {
            for p in l {
                let ip = p
                    .get("ip")
                    .and_then(Value::str)
                    .and_then(|s| s.parse::<IpAddr>().ok());
                let port = p.get("port").and_then(Value::int);
                if let (Some(ip), Some(port)) = (ip, port) {
                    if port > 0 && port < 65536 {
                        peers.push(SocketAddr::new(ip, port as u16));
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(b) = v.get("peers6").and_then(Value::bytes) {
        peers.extend(parse_compact_v6(b));
    }
    let count = |k: &str| {
        v.get(k)
            .and_then(Value::int)
            .map(|n| n.clamp(0, u32::MAX as i64) as u32)
    };
    Ok(Response {
        interval,
        peers,
        seeders: count("complete"),
        leechers: count("incomplete"),
    })
}

fn udp_announce(url: &str, a: &Announce) -> Result<Response> {
    let u = parse_url(url)?;
    let addr = resolve(&u.host, u.port)?[0];
    let sock = UdpSocket::bind(if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    sock.connect(addr)?;
    let mut buf = [0u8; 2048];

    let transact = |req: &[u8], txid: u32, min_len: usize, buf: &mut [u8]| -> Result<usize> {
        for attempt in 0..2 {
            sock.send(req)?;
            sock.set_read_timeout(Some(Duration::from_secs(5 << attempt)))?;
            loop {
                match sock.recv(buf) {
                    Ok(n) if n >= 8 && buf[4..8] == txid.to_be_bytes() => {
                        let action = u32::from_be_bytes(buf[..4].try_into().unwrap());
                        if action == 3 {
                            bail!("tracker: {}", String::from_utf8_lossy(&buf[8..n]));
                        }
                        if n < min_len {
                            bail!("short udp tracker response");
                        }
                        return Ok(n);
                    }
                    Ok(_) => continue,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        bail!("udp tracker timed out")
    };

    let txid = rand_u64() as u32;
    let mut req = Vec::with_capacity(98);
    req.extend_from_slice(&0x41727101980u64.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&txid.to_be_bytes());
    transact(&req, txid, 16, &mut buf)?;
    let conn_id = &buf[8..16].to_vec();

    let txid = rand_u64() as u32;
    let mut req = Vec::with_capacity(98);
    req.extend_from_slice(conn_id);
    req.extend_from_slice(&1u32.to_be_bytes());
    req.extend_from_slice(&txid.to_be_bytes());
    req.extend_from_slice(a.info_hash);
    req.extend_from_slice(a.peer_id);
    req.extend_from_slice(&a.downloaded.to_be_bytes());
    req.extend_from_slice(&a.left.to_be_bytes());
    req.extend_from_slice(&a.uploaded.to_be_bytes());
    let ev: u32 = match a.event {
        Event::None => 0,
        Event::Completed => 1,
        Event::Started => 2,
    };
    req.extend_from_slice(&ev.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes()); // ip
    req.extend_from_slice(&(rand_u64() as u32).to_be_bytes()); // key
    req.extend_from_slice(&(-1i32).to_be_bytes()); // num_want
    req.extend_from_slice(&a.port.to_be_bytes());
    let n = transact(&req, txid, 20, &mut buf)?;
    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as u64;
    let leechers = u32::from_be_bytes(buf[12..16].try_into().unwrap());
    let seeders = u32::from_be_bytes(buf[16..20].try_into().unwrap());
    let peers = if addr.is_ipv4() {
        parse_compact_v4(&buf[20..n])
    } else {
        parse_compact_v6(&buf[20..n])
    };
    Ok(Response {
        interval,
        peers,
        seeders: Some(seeders),
        leechers: Some(leechers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        let u = parse_url("http://t.example.com:6969/announce?x=1").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("t.example.com", 6969, "/announce?x=1")
        );
        let u = parse_url("https://[::1]/a").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.https), ("::1", 443, true));
        assert!(parse_url("udp://x").is_err());
    }
    #[test]
    fn http_body() {
        let r = parse_http_response(b"d8:intervali900e5:peers6:\x7f\x00\x00\x01\x1a\xe1e").unwrap();
        assert_eq!(r.interval, 900);
        assert_eq!(r.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
        assert_eq!(dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"), b"abcde");
    }
}
