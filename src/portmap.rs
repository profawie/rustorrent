//! Automatic port forwarding on the home router: NAT-PMP (RFC 6886) first, then UPnP IGD.
//! Mappings are renewed periodically and removed again on shutdown.

use crate::bail;
use crate::util::Result;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LEASE: u32 = 3600;
const RENEW: Duration = Duration::from_secs(1800);
const RETRY: Duration = Duration::from_secs(300);
const PROTOCOLS: [&str; 2] = ["TCP", "UDP"];

#[derive(Clone)]
enum Mapping {
    NatPmp(Ipv4Addr),
    Upnp(Igd),
}

#[derive(Clone, Debug, PartialEq)]
struct Igd {
    /// Absolute control URL of the WANIPConnection / WANPPPConnection service.
    control: String,
    service: String,
}

pub struct PortMapper {
    port: u16,
    status: Mutex<String>,
    stop: AtomicBool,
    mapping: Mutex<Option<Mapping>>,
}

impl PortMapper {
    pub fn start(port: u16) -> Arc<PortMapper> {
        let pm = Arc::new(PortMapper {
            port,
            status: Mutex::new("forwarding\u{2026}".into()),
            stop: AtomicBool::new(false),
            mapping: Mutex::new(None),
        });
        let p = pm.clone();
        crate::util::spawn(move || p.run());
        pm
    }

    pub fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_status(&self, s: String) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    fn run(&self) {
        while !self.stop.load(SeqCst) {
            let res = self.map_once();
            if self.stop.load(SeqCst) {
                break;
            }
            let wait = match res {
                Ok(msg) => {
                    self.set_status(msg);
                    RENEW
                }
                Err(e) => {
                    self.set_status(format!("not forwarded: {}", e));
                    RETRY
                }
            };
            crate::util::sleep_until_stop(wait, || self.stop.load(SeqCst));
        }
    }

    fn map_once(&self) -> Result<String> {
        let port = self.port;
        // Renew the mapping we already have, if any.
        let current = self.mapping.lock().unwrap().clone();
        let mut errors = Vec::new();
        if !matches!(current, Some(Mapping::Upnp(_))) {
            let gw = match current {
                Some(Mapping::NatPmp(gw)) => Some(gw),
                _ => gateway(),
            };
            if let Some(gw) = gw {
                match natpmp_map(gw, port, LEASE) {
                    Ok(ext) => {
                        *self.mapping.lock().unwrap() = Some(Mapping::NatPmp(gw));
                        return Ok(match ext {
                            Some(ip) => format!("forwarded by NAT-PMP ({})", ip),
                            None => "forwarded by NAT-PMP".to_string(),
                        });
                    }
                    Err(e) => errors.push(format!("NAT-PMP: {}", e)),
                }
            }
        }
        let igds = match current {
            Some(Mapping::Upnp(igd)) => vec![igd],
            _ => upnp_discover(),
        };
        if igds.is_empty() {
            errors.push("no UPnP router found".into());
        }
        for igd in igds {
            if self.stop.load(SeqCst) {
                break;
            }
            match upnp_map(&igd, port) {
                Ok(()) => {
                    let ext = upnp_external_ip(&igd).ok();
                    *self.mapping.lock().unwrap() = Some(Mapping::Upnp(igd));
                    return Ok(match ext {
                        Some(ip) => format!("forwarded by UPnP ({})", ip),
                        None => "forwarded by UPnP".to_string(),
                    });
                }
                Err(e) => errors.push(format!("UPnP: {}", e)),
            }
        }
        *self.mapping.lock().unwrap() = None;
        bail!("{}", errors.last().cloned().unwrap_or_default())
    }

    /// Stop renewing and remove the mapping from the router.
    pub fn stop(&self) {
        self.stop.store(true, SeqCst);
        let m = self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match m {
            Some(Mapping::NatPmp(gw)) => {
                let _ = natpmp_map(gw, self.port, 0);
            }
            Some(Mapping::Upnp(igd)) => {
                for proto in PROTOCOLS {
                    let body = format!(
                        "<NewRemoteHost></NewRemoteHost><NewExternalPort>{}</NewExternalPort><NewProtocol>{}</NewProtocol>",
                        self.port, proto
                    );
                    let _ = soap(&igd, "DeletePortMapping", &body);
                }
            }
            None => {}
        }
    }
}

// ---- default gateway ----

#[cfg(windows)]
fn gateway() -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
    unsafe {
        let mut row: MIB_IPFORWARDROW = std::mem::zeroed();
        // Route towards a public address (8.8.8.8, in network byte order).
        let dest = u32::from_ne_bytes([8, 8, 8, 8]);
        if GetBestRoute(dest, 0, &mut row) != 0 {
            return None;
        }
        let ip = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
        (!ip.is_unspecified()).then_some(ip)
    }
}

#[cfg(not(windows))]
fn gateway() -> Option<Ipv4Addr> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in routes.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let gw = u32::from_str_radix(f[2], 16).ok()?;
            return Some(Ipv4Addr::from(gw.to_le_bytes()));
        }
    }
    None
}

/// Our LAN address as seen when talking to `to` (no packet is sent).
fn local_ip(to: SocketAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(to).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

// ---- NAT-PMP ----

fn natpmp_request(sock: &UdpSocket, req: &[u8], want_op: u8) -> Result<Vec<u8>> {
    let mut buf = [0u8; 64];
    for attempt in 0..3 {
        sock.send(req)?;
        sock.set_read_timeout(Some(Duration::from_millis(250 << attempt)))?;
        let deadline = Instant::now() + Duration::from_millis(250 << attempt);
        while Instant::now() < deadline {
            match sock.recv(&mut buf) {
                Ok(n) if n >= 8 && buf[0] == 0 && buf[1] == want_op => {
                    let code = u16::from_be_bytes([buf[2], buf[3]]);
                    if code != 0 {
                        bail!("router refused (code {})", code);
                    }
                    return Ok(buf[..n].to_vec());
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }
    bail!("no answer from router")
}

/// Map TCP and UDP `port` for `lifetime` seconds (0 deletes). Returns the external IP if known.
fn natpmp_map(gw: Ipv4Addr, port: u16, lifetime: u32) -> Result<Option<Ipv4Addr>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect((gw, 5351))?;
    for op in [2u8, 1] {
        let mut req = vec![0, op, 0, 0];
        req.extend_from_slice(&port.to_be_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        req.extend_from_slice(&lifetime.to_be_bytes());
        let r = natpmp_request(&sock, &req, 128 + op)?;
        if r.len() >= 12 && lifetime > 0 {
            let ext = u16::from_be_bytes([r[10], r[11]]);
            if ext != port {
                bail!("router gave port {} instead of {}", ext, port);
            }
        }
    }
    if lifetime == 0 {
        return Ok(None);
    }
    Ok(natpmp_request(&sock, &[0, 0], 128)
        .ok()
        .filter(|r| r.len() >= 12)
        .map(|r| Ipv4Addr::new(r[8], r[9], r[10], r[11])))
}

// ---- UPnP IGD ----

const SEARCH_TARGETS: [&str; 3] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
];

fn upnp_discover() -> Vec<Igd> {
    let mut locations: Vec<String> = Vec::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let dest: SocketAddr = "239.255.255.250:1900".parse().unwrap();
    for st in SEARCH_TARGETS {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\r\n",
            st
        );
        let _ = sock.send_to(msg.as_bytes(), dest);
    }
    let end = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 2048];
    while Instant::now() < end {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("location") {
                        let v = v.trim().to_string();
                        if !locations.contains(&v) {
                            locations.push(v);
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for loc in locations {
        if let Ok(igd) = igd_from_location(&loc) {
            if !out.contains(&igd) {
                out.push(igd);
            }
        }
    }
    out
}

fn igd_from_location(location: &str) -> Result<Igd> {
    let (status, xml) = http(location, "GET", &[], "")?;
    if status != 200 {
        bail!("description status {}", status);
    }
    parse_description(location, &xml).ok_or_else(|| "no WAN connection service".into())
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{}>", name);
    let close = format!("</{}>", name);
    let s = xml.find(&open)? + open.len();
    let e = s + xml[s..].find(&close)?;
    Some(xml[s..e].trim())
}

fn parse_description(location: &str, xml: &str) -> Option<Igd> {
    let base = tag(xml, "URLBase")
        .filter(|b| !b.is_empty())
        .unwrap_or(location);
    // Prefer WANIPConnection over WANPPPConnection.
    for want in ["WANIPConnection", "WANPPPConnection"] {
        for block in xml.split("<service>").skip(1) {
            let block = block.split("</service>").next().unwrap_or("");
            let Some(st) = tag(block, "serviceType") else {
                continue;
            };
            if st.contains(want) {
                let ctl = tag(block, "controlURL")?;
                return Some(Igd {
                    control: resolve_url(base, ctl),
                    service: st.to_string(),
                });
            }
        }
    }
    None
}

fn resolve_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let host = rest.split('/').next().unwrap_or(rest);
    if rel.starts_with('/') {
        format!("{}://{}{}", scheme, host, rel)
    } else {
        format!("{}://{}/{}", scheme, host, rel)
    }
}

/// Minimal HTTP/1.1 client for the router (plain http only). Returns (status, body).
fn http(url: &str, method: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, String)> {
    let rest = url.strip_prefix("http://").ok_or("not an http url")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') {
        hostport.to_socket_addrs()?.next()
    } else {
        (hostport, 80).to_socket_addrs()?.next()
    }
    .ok_or("cannot resolve router")?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: rustorrent/{}\r\n",
        method,
        path,
        hostport,
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    if method == "POST" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    s.take(1 << 20).read_to_end(&mut resp)?;
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad http response")?;
    let head = String::from_utf8_lossy(&resp[..end]).to_ascii_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut body = resp[end + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        body = crate::tracker::dechunk(&body);
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn soap(igd: &Igd, action: &str, args: &str) -> Result<String> {
    let envelope = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{a} xmlns:u=\"{s}\">{args}</u:{a}></s:Body></s:Envelope>\r\n",
        a = action,
        s = igd.service,
        args = args
    );
    let soap_action = format!("\"{}#{}\"", igd.service, action);
    let (status, body) = http(
        &igd.control,
        "POST",
        &[
            ("Content-Type", "text/xml; charset=\"utf-8\""),
            ("SOAPAction", &soap_action),
        ],
        &envelope,
    )?;
    if status != 200 {
        let code = tag(&body, "errorCode").unwrap_or("?").to_string();
        let desc = tag(&body, "errorDescription").unwrap_or("").to_string();
        bail!("error {} {}", code, desc);
    }
    Ok(body)
}

fn upnp_map(igd: &Igd, port: u16) -> Result<()> {
    let ctl_addr = igd
        .control
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .and_then(|hp| {
            if hp.contains(':') {
                hp.to_socket_addrs().ok()?.next()
            } else {
                (hp, 80).to_socket_addrs().ok()?.next()
            }
        })
        .ok_or("bad control url")?;
    let ip = local_ip(ctl_addr).ok_or("no local address")?;
    for proto in PROTOCOLS {
        let args = |lease: u32| {
            format!(
                "<NewRemoteHost></NewRemoteHost><NewExternalPort>{p}</NewExternalPort><NewProtocol>{proto}</NewProtocol><NewInternalPort>{p}</NewInternalPort><NewInternalClient>{ip}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>rustorrent</NewPortMappingDescription><NewLeaseDuration>{lease}</NewLeaseDuration>",
                p = port,
                proto = proto,
                ip = ip,
                lease = lease
            )
        };
        if let Err(e) = soap(igd, "AddPortMapping", &args(LEASE)) {
            // 725 = OnlyPermanentLeasesSupported.
            if e.0.contains("725") {
                soap(igd, "AddPortMapping", &args(0))?;
            } else if e.0.contains("718") {
                bail!("port {} is used by another device", port);
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

fn upnp_external_ip(igd: &Igd) -> Result<String> {
    let body = soap(igd, "GetExternalIPAddress", "")?;
    tag(&body, "NewExternalIPAddress")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "no external address".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const DESC: &str = r#"<?xml version="1.0"?><root><device><deviceList><device><serviceList>
<service><serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType><controlURL>/ctl/CmnIfCfg</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service>
</serviceList></device></deviceList></device></root>"#;

    #[test]
    fn description_and_urls() {
        let igd = parse_description("http://192.168.1.1:5000/rootDesc.xml", DESC).unwrap();
        assert_eq!(igd.control, "http://192.168.1.1:5000/ctl/IPConn");
        assert_eq!(
            igd.service,
            "urn:schemas-upnp-org:service:WANIPConnection:1"
        );
        assert_eq!(resolve_url("http://h:1/a/b.xml", "c"), "http://h:1/c");
        assert_eq!(resolve_url("http://h:1/", "http://x/y"), "http://x/y");
    }

    /// A fake router: serves the description and accepts AddPortMapping for TCP and UDP.
    #[test]
    fn upnp_against_fake_router() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                s.set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let t = String::from_utf8_lossy(&got);
                    if t.starts_with("GET") && t.contains("\r\n\r\n") {
                        break;
                    }
                    if t.contains("</s:Envelope>") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&got).into_owned();
                let body = if req.starts_with("GET") {
                    DESC.to_string()
                } else if req.contains("GetExternalIPAddress") {
                    "<NewExternalIPAddress>203.0.113.7</NewExternalIPAddress>".to_string()
                } else {
                    "<ok/>".to_string()
                };
                seen2.lock().unwrap().push(req);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let igd = igd_from_location(&format!("http://127.0.0.1:{}/desc.xml", port)).unwrap();
        upnp_map(&igd, 51413).unwrap();
        assert_eq!(upnp_external_ip(&igd).unwrap(), "203.0.113.7");
        let seen = seen.lock().unwrap();
        let adds: Vec<&String> = seen
            .iter()
            .filter(|r| r.contains("AddPortMapping"))
            .collect();
        assert_eq!(adds.len(), 2);
        assert!(adds[0].contains("<NewProtocol>TCP</NewProtocol>"));
        assert!(adds[1].contains("<NewProtocol>UDP</NewProtocol>"));
        assert!(adds[0].contains("<NewExternalPort>51413</NewExternalPort>"));
        assert!(adds[0].contains(
            "SOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\""
        ));
    }
}
