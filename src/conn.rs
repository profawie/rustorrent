//! Peer wire protocol (BEP 3), extension protocol (BEP 10), ut_metadata (BEP 9) and ut_pex (BEP 11),
//! optionally wrapped in MSE/PE encryption.

use crate::bail;
use crate::bencode::{self, Value, dict};
use crate::limit;
use crate::mse::{
    self, CRYPTO_PLAIN, CRYPTO_RC4, ENC_DISABLE, ENC_REQUIRE, PeerReader, PeerWriter,
};
use crate::torrent::{
    BLOCK, Block, Ctx, PF_CHOKED, PF_INTERESTED, PF_PEER_INTERESTED, PF_UNCHOKED_PEER, PeerLive,
    Status, Torrent,
};
use crate::tracker::{parse_compact_v4, parse_compact_v6};
use crate::util::Result;
use std::io::{BufReader, Read};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

const PSTR: &[u8; 20] = b"\x13BitTorrent protocol";
const UT_METADATA: u8 = 1;
const UT_PEX: u8 = 2;
const MAX_FRAME: usize = 1 << 21;

pub struct Handshake {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub ext: bool,
}

pub fn handshake_bytes(info_hash: &[u8; 20], peer_id: &[u8; 20], dht: bool) -> [u8; 68] {
    let mut h = [0u8; 68];
    h[..20].copy_from_slice(PSTR);
    h[25] |= 0x10; // extension protocol
    if dht {
        h[27] |= 0x01;
    }
    h[28..48].copy_from_slice(info_hash);
    h[48..68].copy_from_slice(peer_id);
    h
}

fn parse_handshake(h: &[u8; 68]) -> Result<Handshake> {
    if &h[..20] != PSTR {
        bail!("not a BitTorrent handshake");
    }
    Ok(Handshake {
        info_hash: h[28..48].try_into().unwrap(),
        peer_id: h[48..68].try_into().unwrap(),
        ext: h[25] & 0x10 != 0,
    })
}

pub fn read_handshake(s: &mut impl Read) -> Result<Handshake> {
    let mut h = [0u8; 68];
    s.read_exact(&mut h)?;
    parse_handshake(&h)
}

/// Client name from an Azureus-style peer id such as `-qB4630-`.
fn client_from_peer_id(id: &[u8; 20]) -> String {
    if id[0] != b'-' || id[7] != b'-' || !id[1..7].iter().all(|c| c.is_ascii_alphanumeric()) {
        return String::new();
    }
    let code = std::str::from_utf8(&id[1..3]).unwrap_or("");
    let name = match code {
        "qB" => "qBittorrent",
        "TR" => "Transmission",
        "DE" => "Deluge",
        "UT" => "\u{b5}Torrent",
        "UM" => "\u{b5}Torrent Mac",
        "BT" => "BitTorrent",
        "LT" | "lt" => "libtorrent",
        "AZ" => "Vuze",
        "BI" => "BiglyBT",
        "RT" => "rustorrent",
        "KT" => "KTorrent",
        "FD" => "Free Download Manager",
        "AG" | "A~" => "Ares",
        "WW" => "WebTorrent",
        _ => code,
    };
    let v: Vec<String> = id[3..7].iter().map(|&c| (c as char).to_string()).collect();
    format!("{} {}", name, v.join("."))
}

/// Make an outgoing connection and run it until it ends. With encryption preferred, a
/// peer that does not understand MSE is retried in plaintext.
pub fn outgoing(t: Arc<Torrent>, addr: SocketAddr, epoch: u64) {
    let mode = t.ctx.encryption.load(Relaxed);
    let attempts: &[bool] = match mode {
        ENC_REQUIRE => &[true],
        ENC_DISABLE => &[false],
        _ => &[true, false],
    };
    let mut handshaked = false;
    for (n, &encrypt) in attempts.iter().enumerate() {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(7));
        if n == 0 {
            t.half_open_done();
        }
        let Ok(s) = stream else { break };
        let mut fallback = false;
        let r = (|| -> Result<()> {
            s.set_read_timeout(Some(Duration::from_secs(15)))?;
            s.set_write_timeout(Some(Duration::from_secs(15)))?;
            let hs = handshake_bytes(&t.info_hash, &t.ctx.peer_id, t.ctx.dht.is_some());
            let (mut r, w) = if encrypt {
                let provide = if mode == ENC_REQUIRE {
                    CRYPTO_RC4
                } else {
                    CRYPTO_RC4 | CRYPTO_PLAIN
                };
                match mse::initiate(&s, &t.info_hash, &hs, provide) {
                    Ok(x) => x,
                    Err(e) => {
                        fallback = e.fallback;
                        return Err(e.err);
                    }
                }
            } else {
                let (r, mut w) = mse::plain(&s)?;
                w.write_all(&hs)?;
                (r, w)
            };
            let peer = read_handshake(&mut r)?;
            if !t.matches_hash(&peer.info_hash) || peer.peer_id == t.ctx.peer_id {
                bail!("handshake mismatch");
            }
            handshaked = true;
            run_registered(t.clone(), s.try_clone()?, r, w, addr, epoch, peer, false)
        })();
        if std::env::var_os("RT_DEBUG").is_some() {
            eprintln!(
                "outgoing {} (enc {}) ended: {:?}",
                addr,
                encrypt,
                r.as_ref().err()
            );
        }
        let _ = s.shutdown(Shutdown::Both);
        if handshaked || !fallback || !t.active(epoch) {
            break;
        }
    }
    t.peer_done(addr, handshaked);
}

/// Handle an accepted connection: plaintext or encrypted handshake, then the peer protocol.
/// `find` maps an info hash (v1 or truncated v2) to its torrent.
pub fn accept(
    s: TcpStream,
    ctx: &Ctx,
    find: impl Fn(&[u8; 20]) -> Option<Arc<Torrent>>,
    all_hashes: impl Fn() -> Vec<[u8; 20]>,
) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let addr = s.peer_addr()?;
    let mode = ctx.encryption.load(Relaxed);
    let mut first = [0u8; 20];
    (&s).read_exact(&mut first)?;
    let (r, w, hs) = if &first == PSTR {
        if mode == ENC_REQUIRE {
            bail!("plaintext connection refused");
        }
        let mut h = [0u8; 68];
        h[..20].copy_from_slice(&first);
        (&s).read_exact(&mut h[20..])?;
        let (r, w) = mse::plain(&s)?;
        (r, w, parse_handshake(&h)?)
    } else {
        if mode == ENC_DISABLE {
            bail!("encrypted connection refused");
        }
        let allow = if mode == ENC_REQUIRE {
            CRYPTO_RC4
        } else {
            CRYPTO_RC4 | CRYPTO_PLAIN
        };
        let (mut r, w, skey) = mse::respond(&s, &first, &all_hashes(), allow)?;
        let hs = read_handshake(&mut r)?;
        if hs.info_hash != skey {
            bail!("handshake does not match encryption key");
        }
        (r, w, hs)
    };
    let t = find(&hs.info_hash).ok_or("unknown torrent")?;
    if t.is_paused() {
        bail!("torrent paused");
    }
    incoming(t, s, r, w, addr, hs)
}

/// Continue an accepted connection whose handshake has already been read.
fn incoming(
    t: Arc<Torrent>,
    s: TcpStream,
    r: PeerReader,
    mut w: PeerWriter,
    addr: SocketAddr,
    hs: Handshake,
) -> Result<()> {
    let Some(epoch) = t.current_gen() else {
        return Ok(());
    };
    if hs.peer_id == t.ctx.peer_id {
        return Ok(());
    }
    // Reply with the hash the peer used (v1, or truncated v2 for hybrid torrents).
    w.write_all(&handshake_bytes(
        &hs.info_hash,
        &t.ctx.peer_id,
        t.ctx.dht.is_some(),
    ))?;
    run_registered(t.clone(), s, r, w, addr, epoch, hs, true)
}

#[allow(clippy::too_many_arguments)]
fn run_registered(
    t: Arc<Torrent>,
    s: TcpStream,
    reader: PeerReader,
    w: PeerWriter,
    addr: SocketAddr,
    epoch: u64,
    hs: Handshake,
    incoming: bool,
) -> Result<()> {
    let live = t
        .register(addr, incoming, w.level())
        .ok_or("duplicate connection")?;
    if let Ok(mut c) = live.client.lock() {
        *c = client_from_peer_id(&hs.peer_id);
    }
    s.set_nodelay(true)?;
    s.set_read_timeout(None)?;
    s.set_write_timeout(Some(Duration::from_secs(60)))?;
    let (tx, rx) = sync_channel::<Vec<u8>>(128);
    // Reader thread: blocking reads, frames handed over a bounded channel.
    crate::util::spawn(move || {
        let mut r = BufReader::with_capacity(64 * 1024, reader);
        loop {
            let mut len = [0u8; 4];
            if r.read_exact(&mut len).is_err() {
                return;
            }
            let len = u32::from_be_bytes(len) as usize;
            if len > MAX_FRAME {
                return;
            }
            let mut buf = vec![0u8; len];
            if r.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                return;
            }
        }
    });
    let id = live.id;
    let mut c = Conn::new(t.clone(), s, w, live, addr, epoch, hs.ext);
    let res = c.run(rx);
    if std::env::var_os("RT_DEBUG").is_some() {
        eprintln!("peer {} closed: {:?}", addr, res.as_ref().err());
    }
    c.cleanup();
    t.unregister(addr, id);
    res
}

struct Conn {
    t: Arc<Torrent>,
    s: TcpStream,
    w: PeerWriter,
    live: Arc<PeerLive>,
    throttled: bool,
    addr: SocketAddr,
    epoch: u64,
    wbuf: Vec<u8>,
    peer_choking: bool,
    am_choking: bool,
    am_interested: bool,
    peer_interested: bool,
    num_pieces: usize,
    bits: Vec<bool>,
    peer_have_count: usize,
    raw_bitfield: Option<Vec<u8>>,
    pending_haves: Vec<u32>,
    avail_registered: bool,
    outstanding: Vec<Block>,
    max_outstanding: usize,
    requests: std::collections::VecDeque<Block>,
    peer_ext: bool,
    peer_ut_metadata: Option<u8>,
    peer_ut_pex: Option<u8>,
    meta_inflight: Option<Instant>,
    have_cursor: usize,
    last_recv: Instant,
    last_send: Instant,
    last_block: Instant,
    last_interest_check: Instant,
    interest_dirty: bool,
}

impl Conn {
    fn new(
        t: Arc<Torrent>,
        s: TcpStream,
        w: PeerWriter,
        live: Arc<PeerLive>,
        addr: SocketAddr,
        epoch: u64,
        peer_ext: bool,
    ) -> Conn {
        let now = Instant::now();
        Conn {
            t,
            s,
            w,
            live,
            throttled: false,
            addr,
            epoch,
            wbuf: Vec::with_capacity(64 * 1024),
            peer_choking: true,
            am_choking: true,
            am_interested: false,
            peer_interested: false,
            num_pieces: 0,
            bits: Vec::new(),
            peer_have_count: 0,
            raw_bitfield: None,
            pending_haves: Vec::new(),
            avail_registered: false,
            outstanding: Vec::new(),
            max_outstanding: 64,
            requests: Default::default(),
            peer_ext,
            peer_ut_metadata: None,
            peer_ut_pex: None,
            meta_inflight: None,
            have_cursor: 0,
            last_recv: now,
            last_send: now,
            last_block: now,
            last_interest_check: now - Duration::from_secs(10),
            interest_dirty: true,
        }
    }

    fn msg(&mut self, id: u8, payload: &[u8]) {
        self.wbuf
            .extend_from_slice(&(payload.len() as u32 + 1).to_be_bytes());
        self.wbuf.push(id);
        self.wbuf.extend_from_slice(payload);
    }

    fn msg_u32s(&mut self, id: u8, vals: &[u32]) {
        let mut p = Vec::with_capacity(vals.len() * 4);
        for v in vals {
            p.extend_from_slice(&v.to_be_bytes());
        }
        self.msg(id, &p);
    }

    fn ext_msg(&mut self, ext_id: u8, payload: &[u8]) {
        let mut p = Vec::with_capacity(payload.len() + 1);
        p.push(ext_id);
        p.extend_from_slice(payload);
        self.msg(20, &p);
    }

    fn flush(&mut self) -> Result<()> {
        if !self.wbuf.is_empty() {
            self.w.write_all(&self.wbuf)?;
            self.wbuf.clear();
            self.last_send = Instant::now();
        }
        Ok(())
    }

    fn send_ext_handshake(&mut self) {
        let mut items = vec![
            (
                "m",
                dict(vec![
                    ("ut_metadata", Value::Int(UT_METADATA as i64)),
                    ("ut_pex", Value::Int(UT_PEX as i64)),
                ]),
            ),
            ("p", Value::Int(self.t.ctx.port as i64)),
            ("reqq", Value::Int(500)),
            (
                "v",
                Value::s(concat!("rustorrent ", env!("CARGO_PKG_VERSION"))),
            ),
        ];
        if let Some(m) = self.t.meta() {
            items.push(("metadata_size", Value::Int(m.info_bytes.len() as i64)));
        }
        let p = dict(items).encode();
        self.ext_msg(0, &p);
    }

    fn run(&mut self, rx: Receiver<Vec<u8>>) -> Result<()> {
        if self.peer_ext {
            self.send_ext_handshake();
        }
        if self.t.meta().is_some() {
            self.on_meta()?;
        }
        if self.t.ctx.dht.is_some() {
            let port = self.t.ctx.port.to_be_bytes();
            self.msg(9, &port);
        }
        self.flush()?;
        loop {
            if !self.t.active(self.epoch) {
                return Ok(());
            }
            let busy = !self.am_choking && !self.requests.is_empty();
            let wait = Duration::from_millis(match (busy, self.throttled) {
                (_, true) => 50,
                (true, false) => 0,
                (false, false) => 250,
            });
            match rx.recv_timeout(wait) {
                Ok(frame) => {
                    self.last_recv = Instant::now();
                    self.handle(&frame)?;
                    for _ in 0..256 {
                        match rx.try_recv() {
                            Ok(f) => self.handle(&f)?,
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("connection closed"),
            }
            self.tick()?;
            self.flush()?;
        }
    }

    /// Called once metadata is known (immediately, or after a magnet fetch completes).
    fn on_meta(&mut self) -> Result<()> {
        let meta = self.t.meta().ok_or("no meta")?;
        self.num_pieces = meta.num_pieces();
        let n = self.num_pieces;
        self.bits = vec![false; n];
        if let Some(raw) = self.raw_bitfield.take() {
            if raw.len() != n.div_ceil(8) {
                bail!("bad bitfield length");
            }
            for i in 0..n {
                self.bits[i] = raw[i / 8] & (0x80 >> (i % 8)) != 0;
            }
        }
        for h in std::mem::take(&mut self.pending_haves) {
            if (h as usize) < n {
                self.bits[h as usize] = true;
            }
        }
        self.peer_have_count = self.bits.iter().filter(|&&b| b).count();
        self.t.avail_add(&self.bits);
        self.avail_registered = true;
        let (bf, cursor, have_count) = self.t.bitfield();
        self.have_cursor = cursor;
        if have_count > 0 {
            self.msg(5, &bf);
        }
        self.interest_dirty = true;
        Ok(())
    }

    fn handle(&mut self, f: &[u8]) -> Result<()> {
        if f.is_empty() {
            return Ok(()); // keep-alive
        }
        let id = f[0];
        let p = &f[1..];
        let u32_at = |i: usize| -> Result<u32> {
            p.get(i..i + 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                .ok_or_else(|| "short message".into())
        };
        match id {
            0 => {
                self.peer_choking = true;
                let o = std::mem::take(&mut self.outstanding);
                self.t.release(&o);
            }
            1 => self.peer_choking = false,
            2 => {
                self.peer_interested = true;
                if self.am_choking && self.t.try_unchoke() {
                    self.am_choking = false;
                    self.msg(1, &[]);
                }
            }
            3 => {
                self.peer_interested = false;
                if !self.am_choking {
                    self.am_choking = true;
                    self.t.release_unchoke();
                    self.requests.clear();
                    self.msg(0, &[]);
                }
            }
            4 => {
                let i = u32_at(0)?;
                if self.num_pieces == 0 {
                    if self.pending_haves.len() < 1 << 20 {
                        self.pending_haves.push(i);
                    }
                } else if (i as usize) < self.num_pieces && !self.bits[i as usize] {
                    self.bits[i as usize] = true;
                    self.peer_have_count += 1;
                    self.t.avail_one(i as usize);
                    self.interest_dirty = true;
                }
            }
            5 => {
                if self.num_pieces == 0 {
                    self.raw_bitfield = Some(p.to_vec());
                } else {
                    if p.len() != self.num_pieces.div_ceil(8) {
                        bail!("bad bitfield length");
                    }
                    self.t.avail_remove(&self.bits);
                    for i in 0..self.num_pieces {
                        self.bits[i] = p[i / 8] & (0x80 >> (i % 8)) != 0;
                    }
                    self.peer_have_count = self.bits.iter().filter(|&&b| b).count();
                    self.t.avail_add(&self.bits);
                    self.interest_dirty = true;
                }
            }
            6 => {
                let blk = (u32_at(0)?, u32_at(4)?, u32_at(8)?);
                if !self.am_choking && blk.2 > 0 && blk.2 <= 2 * BLOCK && self.requests.len() < 1024
                {
                    self.requests.push_back(blk);
                }
            }
            7 => {
                let (index, begin) = (u32_at(0)?, u32_at(4)?);
                let data = &p[8.min(p.len())..];
                let blk = (index, begin, data.len() as u32);
                if let Some(pos) = self.outstanding.iter().position(|b| *b == blk) {
                    self.outstanding.swap_remove(pos);
                }
                self.last_block = Instant::now();
                self.t.downloaded.fetch_add(data.len() as u64, Relaxed);
                self.live.down.fetch_add(data.len() as u64, Relaxed);
                self.t.on_block(index, begin, data)?;
            }
            8 => {
                let blk = (u32_at(0)?, u32_at(4)?, u32_at(8)?);
                self.requests.retain(|b| *b != blk);
            }
            9 => {
                if let (Some(dht), Some(b)) = (&self.t.ctx.dht, p.get(..2)) {
                    let port = u16::from_be_bytes([b[0], b[1]]);
                    if port != 0 {
                        dht.add_node(SocketAddr::new(self.addr.ip(), port));
                    }
                }
            }
            20 => self.handle_ext(p)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_ext(&mut self, p: &[u8]) -> Result<()> {
        let Some((&ext_id, body)) = p.split_first() else {
            return Ok(());
        };
        match ext_id {
            0 => {
                let v = bencode::decode(body)?;
                if let Some(m) = v.get("m") {
                    self.peer_ut_metadata = m
                        .get("ut_metadata")
                        .and_then(Value::int)
                        .filter(|&i| i > 0 && i < 256)
                        .map(|i| i as u8);
                    self.peer_ut_pex = m
                        .get("ut_pex")
                        .and_then(Value::int)
                        .filter(|&i| i > 0 && i < 256)
                        .map(|i| i as u8);
                }
                if let Some(name) = v.get("v").and_then(Value::str) {
                    if let Ok(mut c) = self.live.client.lock() {
                        *c = name.chars().filter(|c| !c.is_control()).take(48).collect();
                    }
                }
                if let Some(q) = v.get("reqq").and_then(Value::int) {
                    self.max_outstanding = q.clamp(8, 250) as usize;
                }
                if let Some(sz) = v.get("metadata_size").and_then(Value::int) {
                    if sz > 0 {
                        self.t.set_metadata_size(sz as usize);
                    }
                }
            }
            UT_METADATA => {
                let (v, used) = bencode::decode_prefix(body)?;
                let piece = v.get("piece").and_then(Value::int).unwrap_or(-1);
                if piece < 0 {
                    return Ok(());
                }
                match v.get("msg_type").and_then(Value::int) {
                    Some(0) => self.serve_metadata(piece as u32),
                    Some(1) => {
                        self.meta_inflight = None;
                        self.t.metadata_piece(piece as u32, &body[used..]);
                    }
                    Some(2) => {
                        self.meta_inflight = None;
                        self.t.metadata_reject(piece as u32);
                    }
                    _ => {}
                }
            }
            UT_PEX => {
                let v = bencode::decode(body)?;
                let mut peers = Vec::new();
                if let Some(b) = v.get("added").and_then(Value::bytes) {
                    peers.extend(parse_compact_v4(b));
                }
                if let Some(b) = v.get("added6").and_then(Value::bytes) {
                    peers.extend(parse_compact_v6(b));
                }
                peers.truncate(200);
                self.t.add_peers(&peers);
            }
            _ => {}
        }
        Ok(())
    }

    fn serve_metadata(&mut self, piece: u32) {
        let Some(ut) = self.peer_ut_metadata else {
            return;
        };
        let meta = self.t.meta();
        let off = piece as usize * 16384;
        match meta {
            Some(m) if off < m.info_bytes.len() => {
                let end = (off + 16384).min(m.info_bytes.len());
                let mut p = dict(vec![
                    ("msg_type", Value::Int(1)),
                    ("piece", Value::Int(piece as i64)),
                    ("total_size", Value::Int(m.info_bytes.len() as i64)),
                ])
                .encode();
                p.extend_from_slice(&m.info_bytes[off..end]);
                self.ext_msg(ut, &p);
            }
            _ => {
                let p = dict(vec![
                    ("msg_type", Value::Int(2)),
                    ("piece", Value::Int(piece as i64)),
                ])
                .encode();
                self.ext_msg(ut, &p);
            }
        }
    }

    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        self.throttled = false;
        if self.num_pieces == 0 {
            if self.t.meta().is_some() {
                self.on_meta()?;
            } else if let Some(ut) = self.peer_ut_metadata {
                if self
                    .meta_inflight
                    .is_none_or(|t| now - t > Duration::from_secs(20))
                {
                    if let Some(piece) = self.t.next_metadata_request() {
                        let p = dict(vec![
                            ("msg_type", Value::Int(0)),
                            ("piece", Value::Int(piece as i64)),
                        ])
                        .encode();
                        self.ext_msg(ut, &p);
                        self.meta_inflight = Some(now);
                    }
                }
            }
        }
        if self.num_pieces > 0 {
            // Announce newly completed pieces.
            let (haves, cursor) = self.t.haves_since(self.have_cursor);
            self.have_cursor = cursor;
            for h in &haves {
                self.msg_u32s(4, &[*h]);
            }
            if !haves.is_empty() {
                self.interest_dirty = true;
            }
            // Interest.
            if self.interest_dirty || now - self.last_interest_check > Duration::from_secs(5) {
                self.interest_dirty = false;
                self.last_interest_check = now;
                let want = self.t.interesting(&self.bits);
                if want && !self.am_interested {
                    self.am_interested = true;
                    self.msg(2, &[]);
                } else if !want && self.am_interested {
                    self.am_interested = false;
                    self.msg(3, &[]);
                }
            }
            // Requests. With a download limit, only request what the budget allows.
            let t = self.t.clone();
            let limits = [&t.ctx.down, &t.down_limit];
            let limit_rate = limits.iter().map(|l| l.rate()).filter(|&r| r > 0).min();
            let max_out = match limit_rate {
                Some(r) => self
                    .max_outstanding
                    .min((r / BLOCK as u64).clamp(2, 64) as usize),
                None => self.max_outstanding,
            };
            if self.am_interested
                && !self.peer_choking
                && self.outstanding.len() < max_out / 2 + 1
                && (limit_rate.is_none() || limit::take(&limits, 0))
            {
                if self.outstanding.is_empty() {
                    self.last_block = now;
                }
                let mut o = std::mem::take(&mut self.outstanding);
                let new = self.t.pick_blocks(&self.bits, &mut o, max_out);
                self.outstanding = o;
                let mut unsent = Vec::new();
                for (i, b, l) in new {
                    if limit_rate.is_none() || limit::take(&limits, l as u64) {
                        self.msg_u32s(6, &[i, b, l]);
                    } else {
                        unsent.push((i, b, l));
                    }
                }
                if !unsent.is_empty() {
                    self.outstanding.retain(|b| !unsent.contains(b));
                    self.t.release(&unsent);
                }
            }
            // Uploads.
            for _ in 0..32 {
                if self.am_choking {
                    break;
                }
                let Some((i, b, l)) = self.requests.pop_front() else {
                    break;
                };
                if !limit::take(&[&t.ctx.up, &t.up_limit], l as u64) {
                    self.requests.push_front((i, b, l));
                    self.throttled = true;
                    break;
                }
                if let Some(data) = self.t.read_block(i, b, l) {
                    let mut p = Vec::with_capacity(8 + data.len());
                    p.extend_from_slice(&i.to_be_bytes());
                    p.extend_from_slice(&b.to_be_bytes());
                    p.extend_from_slice(&data);
                    self.msg(7, &p);
                    self.t.uploaded.fetch_add(l as u64, Relaxed);
                    self.live.up.fetch_add(l as u64, Relaxed);
                    if self.wbuf.len() > 256 * 1024 {
                        break;
                    }
                }
            }
            // We have everything we want and the peer needs nothing from us.
            if self.peer_have_count == self.num_pieces && self.t.is_finished() {
                bail!("both seeds");
            }
            self.live.have.store(self.peer_have_count, Relaxed);
        }
        // Peer stopped sending blocks we asked for.
        if !self.outstanding.is_empty() && now - self.last_block > Duration::from_secs(60) {
            bail!("peer snubbed us");
        }
        if now - self.last_recv > Duration::from_secs(150) {
            bail!("peer timed out");
        }
        if now - self.last_send > Duration::from_secs(90) {
            self.wbuf.extend_from_slice(&[0, 0, 0, 0]);
        }
        let mut flags = 0;
        for (on, bit) in [
            (self.am_interested, PF_INTERESTED),
            (self.peer_choking, PF_CHOKED),
            (self.peer_interested, PF_PEER_INTERESTED),
            (!self.am_choking, PF_UNCHOKED_PEER),
        ] {
            if on {
                flags |= bit;
            }
        }
        self.live.flags.store(flags, Relaxed);
        let status = self.t.lock().status.clone();
        if matches!(status, Status::Error(_) | Status::Paused) {
            bail!("torrent stopped");
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        let o = std::mem::take(&mut self.outstanding);
        self.t.release(&o);
        if self.avail_registered {
            self.t.avail_remove(&self.bits);
        }
        if !self.am_choking {
            self.t.release_unchoke();
        }
        let _ = self.s.shutdown(Shutdown::Both);
    }
}
