//! Minimal Mainline DHT (BEP 5) node: finds peers for torrents and answers basic queries.

use crate::bencode::{self, Value, dict};
use crate::torrent::Torrent;
use crate::tracker::parse_compact_v4;
use crate::util::{rand_bytes, sha1};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const BOOTSTRAP: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];
const MAX_NODES: usize = 2000;
const K: usize = 8;
const ALPHA: usize = 6;

type Id = [u8; 20];

fn xor(a: &Id, b: &Id) -> Id {
    let mut o = [0u8; 20];
    for i in 0..20 {
        o[i] = a[i] ^ b[i];
    }
    o
}

struct Node {
    id: Id,
    addr: SocketAddr,
    last: Instant,
    fails: u8,
}

#[derive(PartialEq)]
enum CState {
    New,
    Sent,
    Ok,
    Fail,
}

struct Cand {
    id: Id,
    addr: SocketAddr,
    state: CState,
    token: Option<Vec<u8>>,
}

struct Search {
    id: u64,
    target: Id,
    torrent: Weak<Torrent>,
    started: Instant,
    cands: Vec<Cand>,
}

struct Tx {
    search: Option<u64>,
    addr: SocketAddr,
    sent: Instant,
}

struct Inner {
    nodes: Vec<Node>,
    pending: HashMap<u16, Tx>,
    next_tx: u16,
    searches: Vec<Search>,
    next_search: u64,
    bootstrap: Vec<SocketAddr>,
    ping_queue: Vec<SocketAddr>,
    last_bootstrap: Option<Instant>,
}

pub struct Dht {
    sock: UdpSocket,
    id: Id,
    port: u16,
    secret: [u8; 16],
    inner: Mutex<Inner>,
}

fn compact_node(id: &Id, addr: &SocketAddr) -> Option<[u8; 26]> {
    let IpAddr::V4(ip) = addr.ip() else {
        return None;
    };
    let mut o = [0u8; 26];
    o[..20].copy_from_slice(id);
    o[20..24].copy_from_slice(&ip.octets());
    o[24..26].copy_from_slice(&addr.port().to_be_bytes());
    Some(o)
}

impl Dht {
    /// Bind the DHT socket and start its thread. `port` is our TCP peer port.
    pub fn start(port: u16, id: Option<Id>, nodes: Vec<SocketAddr>) -> Option<Arc<Dht>> {
        let sock = UdpSocket::bind(("0.0.0.0", port))
            .or_else(|_| UdpSocket::bind("0.0.0.0:0"))
            .ok()?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .ok()?;
        let dht = Arc::new(Dht {
            sock,
            id: id.unwrap_or_else(rand_bytes::<20>),
            port,
            secret: rand_bytes::<16>(),
            inner: Mutex::new(Inner {
                nodes: Vec::new(),
                pending: HashMap::new(),
                next_tx: (crate::util::rand_u64() & 0xffff) as u16,
                searches: Vec::new(),
                next_search: 1,
                bootstrap: Vec::new(),
                ping_queue: nodes,
                last_bootstrap: None,
            }),
        });
        let d = dht.clone();
        std::thread::spawn(move || d.run());
        Some(dht)
    }

    pub fn id(&self) -> Id {
        self.id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn node_count(&self) -> usize {
        self.lock().nodes.iter().filter(|n| n.fails == 0).count()
    }

    pub fn export_nodes(&self) -> Vec<SocketAddr> {
        let inner = self.lock();
        let mut v: Vec<&Node> = inner.nodes.iter().filter(|n| n.fails == 0).collect();
        v.sort_by_key(|n| std::cmp::Reverse(n.last));
        v.into_iter().take(300).map(|n| n.addr).collect()
    }

    /// A peer told us its DHT port; ping it so it can join our table.
    pub fn add_node(&self, addr: SocketAddr) {
        let mut inner = self.lock();
        if inner.ping_queue.len() < 200 && !inner.nodes.iter().any(|n| n.addr == addr) {
            inner.ping_queue.push(addr);
        }
    }

    /// Start a peer lookup for `info_hash`; found peers are fed to the torrent.
    pub fn search(&self, info_hash: Id, torrent: Weak<Torrent>) {
        let mut inner = self.lock();
        if inner.searches.iter().any(|s| s.target == info_hash) {
            return;
        }
        Self::new_search(&mut inner, info_hash, torrent);
    }

    fn new_search(inner: &mut Inner, target: Id, torrent: Weak<Torrent>) {
        let mut nodes: Vec<&Node> = inner.nodes.iter().filter(|n| n.fails < 2).collect();
        nodes.sort_by_key(|n| xor(&n.id, &target));
        let mut cands: Vec<Cand> = nodes
            .iter()
            .take(32)
            .map(|n| Cand {
                id: n.id,
                addr: n.addr,
                state: CState::New,
                token: None,
            })
            .collect();
        if cands.len() < K {
            let far = xor(&target, &[0xff; 20]);
            for a in &inner.bootstrap {
                cands.push(Cand {
                    id: far,
                    addr: *a,
                    state: CState::New,
                    token: None,
                });
            }
        }
        let id = inner.next_search;
        inner.next_search += 1;
        inner.searches.push(Search {
            id,
            target,
            torrent,
            started: Instant::now(),
            cands,
        });
    }

    fn send_query(
        &self,
        inner: &mut Inner,
        addr: SocketAddr,
        q: &str,
        args: Vec<(&str, Value)>,
        search: Option<u64>,
    ) {
        let tx = inner.next_tx;
        inner.next_tx = inner.next_tx.wrapping_add(1);
        let mut a = vec![("id", Value::b(&self.id))];
        a.extend(args);
        let msg = dict(vec![
            ("a", dict(a)),
            ("q", Value::s(q)),
            ("t", Value::b(&tx.to_be_bytes())),
            ("v", Value::s("RT01")),
            ("y", Value::s("q")),
        ]);
        if self.sock.send_to(&msg.encode(), addr).is_ok() {
            inner.pending.insert(
                tx,
                Tx {
                    search,
                    addr,
                    sent: Instant::now(),
                },
            );
        }
    }

    fn token_for(&self, ip: &IpAddr) -> Vec<u8> {
        let mut d = self.secret.to_vec();
        d.extend_from_slice(ip.to_string().as_bytes());
        sha1(&d)[..8].to_vec()
    }

    fn touch_node(inner: &mut Inner, id: Id, addr: SocketAddr, my_id: &Id) {
        if id == *my_id || !addr.is_ipv4() || addr.port() == 0 {
            return;
        }
        let now = Instant::now();
        if let Some(n) = inner.nodes.iter_mut().find(|n| n.addr == addr) {
            n.id = id;
            n.last = now;
            n.fails = 0;
            return;
        }
        if inner.nodes.len() >= MAX_NODES {
            // Replace the worst (most failures, then oldest).
            if let Some((i, _)) = inner
                .nodes
                .iter()
                .enumerate()
                .max_by_key(|(_, n)| (n.fails, std::cmp::Reverse(n.last)))
            {
                inner.nodes.swap_remove(i);
            }
        }
        inner.nodes.push(Node {
            id,
            addr,
            last: now,
            fails: 0,
        });
    }

    fn closest_compact(inner: &Inner, target: &Id) -> Vec<u8> {
        let mut v: Vec<&Node> = inner.nodes.iter().filter(|n| n.fails == 0).collect();
        v.sort_by_key(|n| xor(&n.id, target));
        v.iter()
            .take(K)
            .filter_map(|n| compact_node(&n.id, &n.addr))
            .flatten()
            .collect()
    }

    fn run(self: Arc<Self>) {
        let mut boot = Vec::new();
        for b in BOOTSTRAP {
            if let Ok(a) = b.to_socket_addrs() {
                boot.extend(a.filter(|a| a.is_ipv4()));
            }
        }
        self.lock().bootstrap = boot;
        let mut buf = [0u8; 2048];
        let mut last_step = Instant::now();
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => self.handle(&buf[..n], from),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {} // e.g. ICMP port unreachable on Windows (WSAECONNRESET)
            }
            if last_step.elapsed() >= Duration::from_millis(200) {
                last_step = Instant::now();
                self.step();
            }
        }
    }

    fn handle(&self, data: &[u8], from: SocketAddr) {
        let Ok(v) = bencode::decode(data) else { return };
        let Some(t) = v.get("t").and_then(Value::bytes) else {
            return;
        };
        let y = v.get("y").and_then(Value::bytes).unwrap_or(b"");
        let mut inner = self.lock();
        match y {
            b"r" | b"e" => {
                if t.len() != 2 {
                    return;
                }
                let tx = u16::from_be_bytes([t[0], t[1]]);
                let Some(txi) = inner.pending.remove(&tx) else {
                    return;
                };
                if txi.addr != from {
                    return;
                }
                let r = v.get("r");
                let id: Option<Id> = r
                    .and_then(|r| r.get("id"))
                    .and_then(Value::bytes)
                    .and_then(|b| b.try_into().ok());
                if y == b"e" || id.is_none() {
                    if let Some(s) = txi
                        .search
                        .and_then(|sid| inner.searches.iter_mut().find(|s| s.id == sid))
                    {
                        if let Some(c) = s.cands.iter_mut().find(|c| c.addr == from) {
                            c.state = CState::Fail;
                        }
                    }
                    return;
                }
                let id = id.unwrap();
                let r = r.unwrap();
                Self::touch_node(&mut inner, id, from, &self.id);
                let Some(sid) = txi.search else { return };
                let my_id = self.id;
                let Some(s) = inner.searches.iter_mut().find(|s| s.id == sid) else {
                    return;
                };
                if let Some(c) = s.cands.iter_mut().find(|c| c.addr == from) {
                    c.state = CState::Ok;
                    c.id = id;
                    c.token = r.get("token").and_then(Value::bytes).map(|b| b.to_vec());
                }
                if let Some(vals) = r.get("values").and_then(Value::list) {
                    let mut peers = Vec::new();
                    for val in vals.iter().take(200) {
                        if let Some(b) = val.bytes() {
                            peers.extend(parse_compact_v4(b));
                        }
                    }
                    if let Some(t) = s.torrent.upgrade() {
                        t.add_peers(&peers);
                    }
                }
                if let Some(nodes) = r.get("nodes").and_then(Value::bytes) {
                    for c in nodes.as_chunks::<26>().0 {
                        let nid: Id = c[..20].try_into().unwrap();
                        let addr = SocketAddr::new(
                            IpAddr::V4(Ipv4Addr::new(c[20], c[21], c[22], c[23])),
                            u16::from_be_bytes([c[24], c[25]]),
                        );
                        if addr.port() == 0
                            || nid == my_id
                            || s.cands.iter().any(|c| c.addr == addr)
                        {
                            continue;
                        }
                        s.cands.push(Cand {
                            id: nid,
                            addr,
                            state: CState::New,
                            token: None,
                        });
                    }
                    let target = s.target;
                    s.cands.sort_by_key(|c| xor(&c.id, &target));
                    s.cands.truncate(64);
                }
            }
            b"q" => {
                let Some(a) = v.get("a") else { return };
                let Some(id) = a
                    .get("id")
                    .and_then(Value::bytes)
                    .and_then(|b| <Id>::try_from(b).ok())
                else {
                    return;
                };
                let q = v.get("q").and_then(Value::bytes).unwrap_or(b"");
                let mut r = vec![("id", Value::b(&self.id))];
                match q {
                    b"ping" | b"announce_peer" => {}
                    b"find_node" | b"get_peers" => {
                        let key = if q == b"find_node" {
                            "target"
                        } else {
                            "info_hash"
                        };
                        let Some(target) = a
                            .get(key)
                            .and_then(Value::bytes)
                            .and_then(|b| <Id>::try_from(b).ok())
                        else {
                            return;
                        };
                        r.push((
                            "nodes",
                            Value::Bytes(Self::closest_compact(&inner, &target)),
                        ));
                        if q == b"get_peers" {
                            r.push(("token", Value::Bytes(self.token_for(&from.ip()))));
                        }
                    }
                    _ => {
                        let e = dict(vec![
                            (
                                "e",
                                Value::List(vec![Value::Int(204), Value::s("Method Unknown")]),
                            ),
                            ("t", Value::b(t)),
                            ("y", Value::s("e")),
                        ]);
                        let _ = self.sock.send_to(&e.encode(), from);
                        return;
                    }
                }
                let resp = dict(vec![
                    ("r", dict(r)),
                    ("t", Value::b(t)),
                    ("y", Value::s("r")),
                ]);
                let _ = self.sock.send_to(&resp.encode(), from);
                Self::touch_node(&mut inner, id, from, &self.id);
            }
            _ => {}
        }
    }

    fn step(&self) {
        let mut inner = self.lock();
        let now = Instant::now();
        // Expire unanswered queries.
        let expired: Vec<u16> = inner
            .pending
            .iter()
            .filter(|(_, t)| now - t.sent > Duration::from_secs(4))
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            let tx = inner.pending.remove(&k).unwrap();
            if let Some(n) = inner.nodes.iter_mut().find(|n| n.addr == tx.addr) {
                n.fails = n.fails.saturating_add(1);
            }
            if let Some(s) = tx
                .search
                .and_then(|sid| inner.searches.iter_mut().find(|s| s.id == sid))
            {
                if let Some(c) = s.cands.iter_mut().find(|c| c.addr == tx.addr) {
                    c.state = CState::Fail;
                }
            }
        }
        inner.nodes.retain(|n| n.fails < 5);
        // Pings for nodes learnt from peers / saved state.
        let pings: Vec<SocketAddr> = inner.ping_queue.drain(..).take(20).collect();
        for a in pings {
            self.send_query(&mut inner, a, "ping", vec![], None);
        }
        // Keep the routing table populated.
        let good = inner.nodes.iter().filter(|n| n.fails == 0).count();
        if good < 50
            && inner
                .last_bootstrap
                .is_none_or(|t| now - t > Duration::from_secs(60))
            && !inner.bootstrap.is_empty()
        {
            inner.last_bootstrap = Some(now);
            if !inner.searches.iter().any(|s| s.target == self.id) {
                Self::new_search(&mut inner, self.id, Weak::new());
            }
        }
        // Advance searches.
        let mut queries: Vec<(SocketAddr, u64, Id)> = Vec::new();
        let mut finished: Vec<u64> = Vec::new();
        for s in inner.searches.iter_mut() {
            let mut inflight = s.cands.iter().filter(|c| c.state == CState::Sent).count();
            for c in s
                .cands
                .iter_mut()
                .filter(|c| c.state != CState::Fail)
                .take(16)
            {
                if inflight >= ALPHA {
                    break;
                }
                if c.state == CState::New {
                    c.state = CState::Sent;
                    inflight += 1;
                    queries.push((c.addr, s.id, s.target));
                }
            }
            let pending_new = s
                .cands
                .iter()
                .filter(|c| c.state != CState::Fail)
                .take(16)
                .any(|c| c.state == CState::New);
            if (inflight == 0 && !pending_new) || now - s.started > Duration::from_secs(60) {
                finished.push(s.id);
            }
        }
        for (addr, sid, target) in queries {
            self.send_query(
                &mut inner,
                addr,
                "get_peers",
                vec![("info_hash", Value::b(&target))],
                Some(sid),
            );
        }
        for sid in finished {
            let pos = inner.searches.iter().position(|s| s.id == sid).unwrap();
            let s = inner.searches.swap_remove(pos);
            if s.torrent.upgrade().is_none() {
                continue;
            }
            // Announce ourselves to the closest nodes that gave us a token.
            for c in s
                .cands
                .iter()
                .filter(|c| c.state == CState::Ok && c.token.is_some())
                .take(K)
            {
                let args = vec![
                    ("implied_port", Value::Int(0)),
                    ("info_hash", Value::b(&s.target)),
                    ("port", Value::Int(self.port as i64)),
                    ("token", Value::Bytes(c.token.clone().unwrap())),
                ];
                self.send_query(&mut inner, c.addr, "announce_peer", args, None);
            }
        }
    }
}
