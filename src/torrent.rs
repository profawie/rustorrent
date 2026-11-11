//! Per-torrent state: piece picking, verification, peer bookkeeping and the main loop.

use crate::conn;
use crate::dht::Dht;
use crate::limit::Limiter;
use crate::metainfo::Metainfo;
use crate::storage::Storage;
use crate::tracker::{self, Announce, Event};
use crate::util::{Error, Result, sleep_until_stop};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const BLOCK: u32 = 16384;
const MAX_HALF_OPEN: usize = 16;
const MAX_UNCHOKED: usize = 16;
const MAX_KNOWN_PEERS: usize = 4000;

const FREE: u8 = 0;
const REQ: u8 = 1;
const WRITING: u8 = 2;
const GOT: u8 = 3;

/// (piece, begin, length)
pub type Block = (u32, u32, u32);

/// Shared session context visible to every torrent.
pub struct Ctx {
    pub peer_id: [u8; 20],
    pub port: u16,
    pub dht: Option<Arc<Dht>>,
    pub conns: AtomicUsize,
    /// Connection limit for the whole session.
    pub max_conns: AtomicUsize,
    /// Connection limit for each torrent.
    pub max_peers: AtomicUsize,
    /// One of `mse::ENC_*`.
    pub encryption: AtomicU8,
    pub down: Limiter,
    pub up: Limiter,
}

impl Ctx {
    pub fn new(peer_id: [u8; 20], port: u16, dht: Option<Arc<Dht>>) -> Ctx {
        Ctx {
            peer_id,
            port,
            dht,
            conns: AtomicUsize::new(0),
            max_conns: AtomicUsize::new(500),
            max_peers: AtomicUsize::new(80),
            encryption: AtomicU8::new(crate::mse::ENC_PREFER),
            down: Limiter::new(0),
            up: Limiter::new(0),
        }
    }
}

/// File priorities.
pub const PRIO_SKIP: u8 = 0;
pub const PRIO_NORMAL: u8 = 1;
pub const PRIO_HIGH: u8 = 2;

/// Live statistics of one connected peer, shared between its connection thread and the GUI.
pub struct PeerLive {
    pub addr: SocketAddr,
    pub id: u64,
    pub incoming: bool,
    /// 0 = plaintext, 1 = encrypted handshake, 2 = fully encrypted.
    pub encryption: u8,
    pub client: Mutex<String>,
    pub have: AtomicUsize,
    pub down: AtomicU64,
    pub up: AtomicU64,
    pub down_rate: AtomicU64,
    pub up_rate: AtomicU64,
    last: Mutex<(u64, u64)>,
    /// Bit flags, see `PF_*`.
    pub flags: AtomicU8,
}

pub const PF_INTERESTED: u8 = 1; // we want data from the peer
pub const PF_CHOKED: u8 = 2; // peer is choking us
pub const PF_PEER_INTERESTED: u8 = 4;
pub const PF_UNCHOKED_PEER: u8 = 8; // we are uploading to the peer

#[derive(Clone, Default)]
pub struct TrackerInfo {
    pub status: String,
    pub peers: usize,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub next: Option<Instant>,
}

pub struct FileInfo {
    /// Index into the metainfo file list (for `set_file_prio`).
    pub index: usize,
    /// Path relative to the save folder.
    pub rel: PathBuf,
    /// Path shown to the user (inside the torrent folder).
    pub path: String,
    pub size: u64,
    pub done: u64,
    pub prio: u8,
}

pub struct PeerInfo {
    pub addr: SocketAddr,
    pub client: String,
    pub flags: String,
    pub progress: f64,
    pub down_rate: u64,
    pub up_rate: u64,
    pub downloaded: u64,
    pub uploaded: u64,
}

pub struct TrackerRow {
    pub url: String,
    pub info: TrackerInfo,
}

/// Everything the details panel shows about one torrent.
pub struct Details {
    pub id: u64,
    pub name: String,
    pub save_dir: PathBuf,
    pub info_hash: [u8; 20],
    pub info_hash_v2: Option<[u8; 20]>,
    pub has_meta: bool,
    pub total_size: u64,
    pub wanted_size: u64,
    pub piece_length: u64,
    pub num_pieces: usize,
    pub have_pieces: usize,
    pub have_bytes: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub down_rate: u64,
    pub up_rate: u64,
    pub down_limit: u64,
    pub up_limit: u64,
    pub status: Status,
    pub added_on: u64,
    pub completed_on: u64,
    pub known_peers: usize,
    /// One entry per piece: 0 missing, 1 downloading, 2 have, 3 not wanted.
    pub pieces: Vec<u8>,
    pub files: Vec<FileInfo>,
    pub peers: Vec<PeerInfo>,
    pub trackers: Vec<TrackerRow>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Status {
    Paused,
    Checking,
    Metadata,
    Downloading,
    Seeding,
    Error(String),
}

struct Partial {
    state: Vec<u8>,
    got: u32,
}

struct PeerEntry {
    fails: u32,
    next_try: Instant,
}

struct MetaFetch {
    buf: Vec<u8>,
    got: Vec<bool>,
    requested: Vec<Option<Instant>>,
}

pub struct State {
    pub meta: Option<Arc<Metainfo>>,
    pub storage: Option<Arc<Storage>>,
    pub name: String,
    pub trackers: Vec<String>,
    pub have: Vec<bool>,
    pub have_count: usize,
    pub have_bytes: u64,
    pub have_seq: Vec<u32>,
    partial: HashMap<u32, Partial>,
    avail: Vec<u16>,
    peers: HashMap<SocketAddr, PeerEntry>,
    connected: HashMap<SocketAddr, Arc<PeerLive>>,
    half_open: usize,
    unchoked: usize,
    pub status: Status,
    pub check_progress: f32,
    pub needs_check: bool,
    metafetch: Option<MetaFetch>,
    pub down_rate: u64,
    pub up_rate: u64,
    last_down: u64,
    last_up: u64,
    pub dirty: bool,
    next_conn_id: u64,
    /// Per-file priority (`PRIO_*`), one per metainfo file.
    pub file_prio: Vec<u8>,
    /// Priorities restored for a magnet whose metadata is not known yet.
    pub pending_prio: Option<Vec<u8>>,
    /// Highest priority of any file touching each piece (0 = not wanted).
    piece_prio: Vec<u8>,
    wanted_count: usize,
    have_wanted_count: usize,
    wanted_bytes: u64,
    have_wanted_bytes: u64,
    pub tracker_info: HashMap<String, TrackerInfo>,
    pub added_on: u64,
    pub completed_on: u64,
}

pub struct Torrent {
    pub id: u64,
    pub info_hash: [u8; 20],
    pub save_dir: PathBuf,
    pub ctx: Arc<Ctx>,
    pub state: Mutex<State>,
    pub downloaded: AtomicU64,
    pub uploaded: AtomicU64,
    pub down_limit: Limiter,
    pub up_limit: Limiter,
    epoch: AtomicU64,
    paused: AtomicBool,
    reannounce: AtomicU64,
}

pub struct Stats {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub progress: f64,
    pub status: Status,
    pub down_rate: u64,
    pub up_rate: u64,
    pub peers: usize,
    pub known: usize,
    pub eta: u64,
    pub uploaded: u64,
}

fn init_meta(st: &mut State, m: Arc<Metainfo>, save_dir: &std::path::Path) {
    let n = m.num_pieces();
    st.name = m.name.clone();
    st.have = vec![false; n];
    st.have_count = 0;
    st.have_bytes = 0;
    st.have_seq.clear();
    st.partial.clear();
    st.avail = vec![0; n];
    st.storage = Some(Arc::new(Storage::new(save_dir, &m)));
    st.file_prio = match st.pending_prio.take() {
        Some(p) if p.len() == m.files.len() => p,
        _ => vec![PRIO_NORMAL; m.files.len()],
    };
    st.meta = Some(m);
    st.metafetch = None;
    recompute_prio(st);
}

/// Derive piece priorities and the wanted/have counters from the file priorities.
fn recompute_prio(st: &mut State) {
    let Some(m) = st.meta.clone() else { return };
    let n = m.num_pieces();
    let mut pp = vec![PRIO_SKIP; n];
    for (fe, &prio) in m.files.iter().zip(&st.file_prio) {
        if fe.pad || fe.length == 0 || prio == PRIO_SKIP {
            continue;
        }
        let first = (fe.offset / m.piece_length) as usize;
        let last = ((fe.offset + fe.length - 1) / m.piece_length) as usize;
        for p in &mut pp[first..=last.min(n - 1)] {
            *p = (*p).max(prio);
        }
    }
    st.wanted_count = 0;
    st.have_wanted_count = 0;
    st.wanted_bytes = 0;
    st.have_wanted_bytes = 0;
    for (i, &p) in pp.iter().enumerate() {
        if p > 0 {
            let len = m.piece_len(i);
            st.wanted_count += 1;
            st.wanted_bytes += len;
            if st.have[i] {
                st.have_wanted_count += 1;
                st.have_wanted_bytes += len;
            }
        }
    }
    st.piece_prio = pp;
    // Partial pieces that are no longer wanted are dropped; their blocks get re-requested
    // if the files are selected again.
    let prio = &st.piece_prio;
    st.partial.retain(|i, _| prio[*i as usize] > 0);
}

fn finished(st: &State) -> bool {
    st.meta.is_some() && st.have_wanted_count == st.wanted_count
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Torrent {
    pub fn new(
        id: u64,
        info_hash: [u8; 20],
        save_dir: PathBuf,
        ctx: Arc<Ctx>,
        meta: Option<Arc<Metainfo>>,
        name: String,
        trackers: Vec<String>,
    ) -> Arc<Torrent> {
        let mut st = State {
            meta: None,
            storage: None,
            name,
            trackers,
            have: Vec::new(),
            have_count: 0,
            have_bytes: 0,
            have_seq: Vec::new(),
            partial: HashMap::new(),
            avail: Vec::new(),
            peers: HashMap::new(),
            connected: HashMap::new(),
            half_open: 0,
            unchoked: 0,
            status: Status::Paused,
            check_progress: 0.0,
            needs_check: false,
            metafetch: None,
            down_rate: 0,
            up_rate: 0,
            last_down: 0,
            last_up: 0,
            dirty: true,
            next_conn_id: 1,
            file_prio: Vec::new(),
            pending_prio: None,
            piece_prio: Vec::new(),
            wanted_count: 0,
            have_wanted_count: 0,
            wanted_bytes: 0,
            have_wanted_bytes: 0,
            tracker_info: HashMap::new(),
            added_on: now_unix(),
            completed_on: 0,
        };
        if let Some(m) = meta {
            init_meta(&mut st, m, &save_dir);
        }
        Arc::new(Torrent {
            id,
            info_hash,
            save_dir,
            ctx,
            state: Mutex::new(st),
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            down_limit: Limiter::new(0),
            up_limit: Limiter::new(0),
            epoch: AtomicU64::new(0),
            paused: AtomicBool::new(true),
            reannounce: AtomicU64::new(0),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking peer thread must not take the whole torrent down with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn active(&self, epoch: u64) -> bool {
        !self.paused.load(Relaxed) && self.epoch.load(Relaxed) == epoch
    }

    pub fn current_gen(&self) -> Option<u64> {
        if self.paused.load(Relaxed) {
            None
        } else {
            Some(self.epoch.load(Relaxed))
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Relaxed)
    }

    /// True if `ih` identifies this torrent (v1 hash, or truncated v2 hash of a hybrid torrent).
    pub fn matches_hash(&self, ih: &[u8; 20]) -> bool {
        *ih == self.info_hash
            || self
                .lock()
                .meta
                .as_ref()
                .is_some_and(|m| m.info_hash_v2.as_ref() == Some(ih))
    }

    pub fn meta(&self) -> Option<Arc<Metainfo>> {
        self.lock().meta.clone()
    }

    pub fn start(self: &Arc<Self>) {
        if !self.paused.load(Relaxed) {
            return;
        }
        {
            let mut st = self.lock();
            st.status = if st.meta.is_none() {
                Status::Metadata
            } else if finished(&st) && !st.needs_check {
                Status::Seeding
            } else {
                Status::Downloading
            };
            for p in st.peers.values_mut() {
                p.fails = 0;
                p.next_try = Instant::now();
            }
        }
        let epoch = self.epoch.fetch_add(1, SeqCst) + 1;
        self.paused.store(false, SeqCst);
        let t = self.clone();
        crate::util::spawn(move || run(t, epoch));
    }

    pub fn pause(&self) {
        self.paused.store(true, SeqCst);
        self.epoch.fetch_add(1, SeqCst);
        let mut st = self.lock();
        if !matches!(st.status, Status::Error(_)) {
            st.status = Status::Paused;
        }
        st.down_rate = 0;
        st.up_rate = 0;
        st.dirty = true;
    }

    fn fail(&self, msg: String) {
        self.paused.store(true, SeqCst);
        self.epoch.fetch_add(1, SeqCst);
        let mut st = self.lock();
        st.status = Status::Error(msg);
        st.dirty = true;
    }

    /// True once every wanted piece is downloaded (all pieces if no files are skipped).
    pub fn is_finished(&self) -> bool {
        finished(&self.lock())
    }

    /// Replace the restored have-bitfield (resume data) and recompute counters.
    pub fn set_have(&self, have: Vec<bool>) {
        let mut st = self.lock();
        let Some(m) = st.meta.clone() else { return };
        st.have_seq = (0..have.len() as u32)
            .filter(|&i| have[i as usize])
            .collect();
        st.have_count = st.have_seq.len();
        st.have_bytes = st.have_seq.iter().map(|&i| m.piece_len(i as usize)).sum();
        st.have = have;
        recompute_prio(&mut st);
    }

    /// Change the priority of some files (indices into the metainfo file list).
    pub fn set_file_prio(&self, files: &[usize], prio: u8) {
        let mut st = self.lock();
        if st.meta.is_none() {
            return;
        }
        for &f in files {
            if let Some(p) = st.file_prio.get_mut(f) {
                *p = prio;
            }
        }
        recompute_prio(&mut st);
        let done = finished(&st);
        if done && st.status == Status::Downloading {
            st.status = Status::Seeding;
            if let Some(s) = &st.storage {
                s.create_empty_files(&st.file_prio);
            }
        } else if !done && st.status == Status::Seeding {
            st.status = Status::Downloading;
            // Seeds we dropped while finished are useful again: retry them now.
            let now = Instant::now();
            for p in st.peers.values_mut() {
                p.next_try = now;
            }
        }
        st.dirty = true;
    }

    pub fn force_reannounce(&self) {
        self.reannounce.fetch_add(1, SeqCst);
    }

    pub fn remove_trackers(&self, urls: &[String]) {
        let mut st = self.lock();
        st.trackers.retain(|t| !urls.contains(t));
        for u in urls {
            st.tracker_info.remove(u);
        }
        st.dirty = true;
    }

    pub fn add_peers(&self, addrs: &[SocketAddr]) {
        let mut st = self.lock();
        let now = Instant::now();
        for a in addrs {
            if st.peers.len() >= MAX_KNOWN_PEERS {
                break;
            }
            if a.port() == 0 || a.ip().is_unspecified() {
                continue;
            }
            st.peers.entry(*a).or_insert(PeerEntry {
                fails: 0,
                next_try: now,
            });
        }
    }

    pub fn add_trackers(&self, urls: &[String]) {
        let mut st = self.lock();
        for u in urls {
            if !st.trackers.contains(u) {
                st.trackers.push(u.clone());
                st.dirty = true;
            }
        }
    }

    // ---- connection bookkeeping ----

    pub fn register(
        &self,
        addr: SocketAddr,
        incoming: bool,
        encryption: u8,
    ) -> Option<Arc<PeerLive>> {
        let mut st = self.lock();
        let max_peers = self.ctx.max_peers.load(Relaxed);
        if st.connected.contains_key(&addr) || st.connected.len() >= max_peers + 10 {
            return None;
        }
        if self.ctx.conns.fetch_add(1, SeqCst) >= self.ctx.max_conns.load(Relaxed) {
            self.ctx.conns.fetch_sub(1, SeqCst);
            return None;
        }
        let id = st.next_conn_id;
        st.next_conn_id += 1;
        let live = Arc::new(PeerLive {
            addr,
            id,
            incoming,
            encryption,
            client: Mutex::new(String::new()),
            have: AtomicUsize::new(0),
            down: AtomicU64::new(0),
            up: AtomicU64::new(0),
            down_rate: AtomicU64::new(0),
            up_rate: AtomicU64::new(0),
            last: Mutex::new((0, 0)),
            flags: AtomicU8::new(PF_CHOKED),
        });
        st.connected.insert(addr, live.clone());
        Some(live)
    }

    pub fn unregister(&self, addr: SocketAddr, id: u64) {
        let mut st = self.lock();
        if st.connected.get(&addr).is_some_and(|p| p.id == id) {
            st.connected.remove(&addr);
            self.ctx.conns.fetch_sub(1, SeqCst);
        }
    }

    pub fn half_open_done(&self) {
        let mut st = self.lock();
        st.half_open = st.half_open.saturating_sub(1);
    }

    /// Record the result of an outgoing connection attempt for retry back-off.
    pub fn peer_done(&self, addr: SocketAddr, handshaked: bool) {
        let mut st = self.lock();
        let now = Instant::now();
        let remove = match st.peers.get_mut(&addr) {
            Some(p) if handshaked => {
                p.fails = 0;
                p.next_try = now + Duration::from_secs(90);
                false
            }
            Some(p) => {
                p.fails += 1;
                p.next_try = now + Duration::from_secs(60 * (p.fails * p.fails) as u64);
                p.fails >= 4
            }
            None => false,
        };
        if remove {
            st.peers.remove(&addr);
        }
    }

    pub fn try_unchoke(&self) -> bool {
        let mut st = self.lock();
        if st.unchoked < MAX_UNCHOKED {
            st.unchoked += 1;
            true
        } else {
            false
        }
    }

    pub fn release_unchoke(&self) {
        let mut st = self.lock();
        st.unchoked = st.unchoked.saturating_sub(1);
    }

    // ---- availability / interest ----

    pub fn avail_add(&self, bits: &[bool]) {
        let mut st = self.lock();
        for (a, &b) in st.avail.iter_mut().zip(bits) {
            if b {
                *a = a.saturating_add(1);
            }
        }
    }

    pub fn avail_remove(&self, bits: &[bool]) {
        let mut st = self.lock();
        for (a, &b) in st.avail.iter_mut().zip(bits) {
            if b {
                *a = a.saturating_sub(1);
            }
        }
    }

    pub fn avail_one(&self, i: usize) {
        let mut st = self.lock();
        if let Some(a) = st.avail.get_mut(i) {
            *a = a.saturating_add(1);
        }
    }

    pub fn interesting(&self, bits: &[bool]) -> bool {
        let st = self.lock();
        st.have
            .iter()
            .zip(bits)
            .zip(&st.piece_prio)
            .any(|((&h, &b), &p)| b && !h && p > 0)
    }

    /// Our have-messages appended after `from`; returns them and the new cursor.
    pub fn haves_since(&self, from: usize) -> (Vec<u32>, usize) {
        let st = self.lock();
        let n = st.have_seq.len();
        (st.have_seq.get(from..).unwrap_or(&[]).to_vec(), n)
    }

    /// Snapshot of our bitfield (packed) and the have cursor at that moment.
    pub fn bitfield(&self) -> (Vec<u8>, usize, usize) {
        let st = self.lock();
        let mut out = vec![0u8; st.have.len().div_ceil(8)];
        for (i, &h) in st.have.iter().enumerate() {
            if h {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        (out, st.have_seq.len(), st.have_count)
    }

    // ---- piece picking ----

    pub fn pick_blocks(
        &self,
        peer_has: &[bool],
        outstanding: &mut Vec<Block>,
        max: usize,
    ) -> Vec<Block> {
        let mut st = self.lock();
        let st = &mut *st;
        let meta = match &st.meta {
            Some(m) => m.clone(),
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        let want = max.saturating_sub(outstanding.len());
        if want == 0 {
            return out;
        }
        let block_len = |piece: u32, b: usize| -> u32 {
            let plen = meta.piece_len(piece as usize);
            (plen - b as u64 * BLOCK as u64).min(BLOCK as u64) as u32
        };
        // 1. Continue pieces already in progress (high priority ones first).
        let mut order: Vec<u32> = st.partial.keys().copied().collect();
        order.sort_by_key(|&i| std::cmp::Reverse(st.piece_prio[i as usize]));
        for idx in order {
            if !peer_has.get(idx as usize).copied().unwrap_or(false) {
                continue;
            }
            let p = st.partial.get_mut(&idx).unwrap();
            for (b, s) in p.state.iter_mut().enumerate() {
                if *s == FREE {
                    *s = REQ;
                    out.push((idx, b as u32 * BLOCK, block_len(idx, b)));
                    if out.len() >= want {
                        outstanding.extend_from_slice(&out);
                        return out;
                    }
                }
            }
        }
        // 2. Start new pieces: highest priority, then rarest first (random tie-break via
        //    rotating start).
        let n = st.have.len();
        let top = st.piece_prio.iter().copied().max().unwrap_or(0);
        while out.len() < want {
            let start = (crate::util::rand_u64() % n as u64) as usize;
            // Key: (priority descending, availability ascending).
            let mut best: Option<(u8, u16, usize)> = None;
            for k in 0..n {
                let i = (start + k) % n;
                let prio = st.piece_prio[i];
                if prio == 0
                    || st.have[i]
                    || !peer_has.get(i).copied().unwrap_or(false)
                    || st.partial.contains_key(&(i as u32))
                {
                    continue;
                }
                let a = st.avail[i];
                if best.is_none_or(|(bp, ba, _)| prio > bp || (prio == bp && a < ba)) {
                    best = Some((prio, a, i));
                    if a <= 1 && prio == top {
                        break;
                    }
                }
            }
            let Some((_, _, i)) = best else { break };
            let nblocks = meta.piece_len(i).div_ceil(BLOCK as u64) as usize;
            let mut p = Partial {
                state: vec![FREE; nblocks],
                got: 0,
            };
            for b in 0..nblocks {
                if out.len() >= want {
                    break;
                }
                p.state[b] = REQ;
                out.push((i as u32, b as u32 * BLOCK, block_len(i as u32, b)));
            }
            st.partial.insert(i as u32, p);
        }
        // 3. End game: every missing wanted piece is in progress; duplicate outstanding requests.
        if out.len() < want && st.have_wanted_count + st.partial.len() >= st.wanted_count {
            'outer: for (&idx, p) in st.partial.iter() {
                if !peer_has.get(idx as usize).copied().unwrap_or(false) {
                    continue;
                }
                for (b, &s) in p.state.iter().enumerate() {
                    if s == REQ {
                        let blk = (idx, b as u32 * BLOCK, block_len(idx, b));
                        if !outstanding.contains(&blk) && !out.contains(&blk) {
                            out.push(blk);
                            if out.len() >= want.min(8) {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        outstanding.extend_from_slice(&out);
        out
    }

    pub fn release(&self, blocks: &[Block]) {
        if blocks.is_empty() {
            return;
        }
        let mut st = self.lock();
        for &(idx, begin, _) in blocks {
            if let Some(p) = st.partial.get_mut(&idx) {
                if let Some(s) = p.state.get_mut((begin / BLOCK) as usize) {
                    if *s == REQ {
                        *s = FREE;
                    }
                }
            }
        }
    }

    /// Store a received block. Returns Err only on a local disk error.
    pub fn on_block(&self, index: u32, begin: u32, data: &[u8]) -> Result<()> {
        let i = index as usize;
        let b = (begin / BLOCK) as usize;
        let (storage, meta) = {
            let mut st = self.lock();
            let meta = match &st.meta {
                Some(m) => m.clone(),
                None => return Ok(()),
            };
            if i >= meta.num_pieces() || st.have[i] || !begin.is_multiple_of(BLOCK) {
                return Ok(());
            }
            let plen = meta.piece_len(i);
            if begin as u64 >= plen || data.len() as u64 != (plen - begin as u64).min(BLOCK as u64)
            {
                return Ok(());
            }
            let Some(p) = st.partial.get_mut(&index) else {
                return Ok(());
            };
            if p.state[b] == WRITING || p.state[b] == GOT {
                return Ok(());
            }
            p.state[b] = WRITING;
            (st.storage.clone().unwrap(), meta)
        };
        let res = storage.write(i, begin as u64, data);
        {
            let mut st = self.lock();
            let Some(p) = st.partial.get_mut(&index) else {
                return Ok(());
            };
            if let Err(e) = res {
                p.state[b] = FREE;
                drop(st);
                let msg = format!("Disk error: {}", e);
                self.fail(msg.clone());
                return Err(Error(msg));
            }
            p.state[b] = GOT;
            p.got += 1;
            if p.got as usize != p.state.len() {
                return Ok(());
            }
            st.partial.remove(&index);
        }
        let plen = meta.piece_len(i);
        let ok = storage
            .hash_piece(i, plen)
            .map(|h| h == meta.hashes[i])
            .unwrap_or(false);
        if ok {
            let mut st = self.lock();
            if !st.have[i] {
                st.have[i] = true;
                st.have_count += 1;
                st.have_bytes += plen;
                st.have_seq.push(index);
                if st.piece_prio[i] > 0 {
                    st.have_wanted_count += 1;
                    st.have_wanted_bytes += plen;
                }
                st.dirty = true;
                if finished(&st) {
                    if st.status == Status::Downloading {
                        st.status = Status::Seeding;
                        st.completed_on = now_unix();
                    }
                    storage.create_empty_files(&st.file_prio);
                }
            }
        }
        Ok(())
    }

    pub fn read_block(&self, index: u32, begin: u32, len: u32) -> Option<Vec<u8>> {
        let (storage, meta) = {
            let st = self.lock();
            if !st.have.get(index as usize).copied().unwrap_or(false) {
                return None;
            }
            (st.storage.clone()?, st.meta.clone()?)
        };
        if begin as u64 + len as u64 > meta.piece_len(index as usize) {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        storage.read(index as usize, begin as u64, &mut buf).ok()?;
        Some(buf)
    }

    // ---- metadata exchange (BEP 9) ----

    pub fn set_metadata_size(&self, size: usize) {
        let mut st = self.lock();
        if st.meta.is_some() || st.metafetch.is_some() || size == 0 || size > 64 << 20 {
            return;
        }
        let n = size.div_ceil(16384);
        st.metafetch = Some(MetaFetch {
            buf: vec![0; size],
            got: vec![false; n],
            requested: vec![None; n],
        });
    }

    pub fn next_metadata_request(&self) -> Option<u32> {
        let mut st = self.lock();
        let mf = st.metafetch.as_mut()?;
        let now = Instant::now();
        for i in 0..mf.got.len() {
            if !mf.got[i] && mf.requested[i].is_none_or(|t| now - t > Duration::from_secs(15)) {
                mf.requested[i] = Some(now);
                return Some(i as u32);
            }
        }
        None
    }

    pub fn metadata_reject(&self, piece: u32) {
        let mut st = self.lock();
        if let Some(mf) = st.metafetch.as_mut() {
            if let Some(r) = mf.requested.get_mut(piece as usize) {
                *r = None;
            }
        }
    }

    pub fn metadata_piece(&self, piece: u32, data: &[u8]) {
        let mut st = self.lock();
        let Some(mf) = st.metafetch.as_mut() else {
            return;
        };
        let p = piece as usize;
        let off = p * 16384;
        if p >= mf.got.len()
            || off + data.len() > mf.buf.len()
            || (p + 1 < mf.got.len() && data.len() != 16384)
        {
            return;
        }
        mf.buf[off..off + data.len()].copy_from_slice(data);
        mf.got[p] = true;
        if !mf.got.iter().all(|&g| g) {
            return;
        }
        let buf = std::mem::take(&mut mf.buf);
        st.metafetch = None;
        if crate::util::sha1(&buf) != self.info_hash {
            return;
        }
        if let Ok(m) = Metainfo::from_info(buf) {
            init_meta(&mut st, Arc::new(m), &self.save_dir);
            let exists = st.storage.as_ref().is_some_and(|s| s.any_file_exists());
            st.needs_check = exists;
            if st.status == Status::Metadata {
                st.status = Status::Downloading;
            }
            st.dirty = true;
        }
    }

    // ---- stats ----

    /// Total size of the selected files.
    fn wanted_file_bytes(st: &State) -> u64 {
        st.meta.as_ref().map_or(0, |m| {
            m.files
                .iter()
                .zip(&st.file_prio)
                .filter(|(f, p)| !f.pad && **p > 0)
                .map(|(f, _)| f.length)
                .sum()
        })
    }

    pub fn stats(&self) -> Stats {
        let st = self.lock();
        let size = Self::wanted_file_bytes(&st);
        let progress = if st.status == Status::Checking {
            st.check_progress as f64
        } else if st.wanted_bytes > 0 {
            st.have_wanted_bytes as f64 / st.wanted_bytes as f64
        } else if st.meta.is_some() {
            1.0
        } else {
            0.0
        };
        let left = st.wanted_bytes.saturating_sub(st.have_wanted_bytes);
        let eta = if st.down_rate > 0 && st.status == Status::Downloading {
            left / st.down_rate
        } else {
            0
        };
        Stats {
            id: self.id,
            name: st.name.clone(),
            size,
            progress,
            status: st.status.clone(),
            down_rate: st.down_rate,
            up_rate: st.up_rate,
            peers: st.connected.len(),
            known: st.peers.len(),
            eta,
            uploaded: self.uploaded.load(Relaxed),
        }
    }

    /// Full snapshot for the details panel.
    pub fn details(&self) -> Details {
        let st = self.lock();
        let meta = st.meta.clone();
        let mut files = Vec::new();
        let mut pieces = Vec::new();
        if let Some(m) = &meta {
            pieces = (0..m.num_pieces())
                .map(|i| {
                    if st.have[i] {
                        2
                    } else if st.partial.contains_key(&(i as u32)) {
                        1
                    } else if st.piece_prio[i] == 0 {
                        3
                    } else {
                        0
                    }
                })
                .collect();
            for (index, (fe, &prio)) in m.files.iter().zip(&st.file_prio).enumerate() {
                if fe.pad {
                    continue;
                }
                let mut done = 0u64;
                if fe.length > 0 {
                    let first = (fe.offset / m.piece_length) as usize;
                    let last = ((fe.offset + fe.length - 1) / m.piece_length) as usize;
                    for p in first..=last.min(m.num_pieces() - 1) {
                        if st.have[p] {
                            let ps = p as u64 * m.piece_length;
                            let pe = ps + m.piece_len(p);
                            done += pe.min(fe.offset + fe.length) - ps.max(fe.offset);
                        }
                    }
                }
                // Show paths inside the torrent folder, as other clients do.
                let rel = fe.path.strip_prefix(&m.name).unwrap_or(&fe.path);
                let path = if rel.as_os_str().is_empty() {
                    fe.path.to_string_lossy().into_owned()
                } else {
                    rel.to_string_lossy().into_owned()
                };
                files.push(FileInfo {
                    index,
                    rel: fe.path.clone(),
                    path,
                    size: fe.length,
                    done,
                    prio,
                });
            }
        }
        let n = pieces.len().max(1);
        let mut peers: Vec<PeerInfo> = st
            .connected
            .values()
            .map(|p| {
                let f = p.flags.load(Relaxed);
                // qBittorrent-style flags: D/d = downloading from peer (unchoked/choked),
                // U/u = uploading to peer (unchoked/choked), I = incoming,
                // E = encrypted, e = encrypted handshake only.
                let mut flags = String::new();
                if f & PF_INTERESTED != 0 {
                    flags.push(if f & PF_CHOKED == 0 { 'D' } else { 'd' });
                }
                if f & PF_PEER_INTERESTED != 0 {
                    flags.push(if f & PF_UNCHOKED_PEER != 0 { 'U' } else { 'u' });
                }
                if p.incoming {
                    flags.push('I');
                }
                match p.encryption {
                    2 => flags.push('E'),
                    1 => flags.push('e'),
                    _ => {}
                }
                PeerInfo {
                    addr: p.addr,
                    client: p.client.lock().map(|c| c.clone()).unwrap_or_default(),
                    flags,
                    progress: p.have.load(Relaxed) as f64 / n as f64,
                    down_rate: p.down_rate.load(Relaxed),
                    up_rate: p.up_rate.load(Relaxed),
                    downloaded: p.down.load(Relaxed),
                    uploaded: p.up.load(Relaxed),
                }
            })
            .collect();
        peers.sort_by(|a, b| {
            (b.down_rate + b.up_rate)
                .cmp(&(a.down_rate + a.up_rate))
                .then(a.addr.cmp(&b.addr))
        });
        let trackers = st
            .trackers
            .iter()
            .map(|u| TrackerRow {
                url: u.clone(),
                info: st.tracker_info.get(u).cloned().unwrap_or_default(),
            })
            .collect();
        Details {
            id: self.id,
            name: st.name.clone(),
            save_dir: self.save_dir.clone(),
            info_hash: self.info_hash,
            info_hash_v2: meta.as_ref().and_then(|m| m.info_hash_v2),
            has_meta: meta.is_some(),
            total_size: meta.as_ref().map_or(0, |m| m.total_length),
            wanted_size: Self::wanted_file_bytes(&st),
            piece_length: meta.as_ref().map_or(0, |m| m.piece_length),
            num_pieces: meta.as_ref().map_or(0, |m| m.num_pieces()),
            have_pieces: st.have_count,
            have_bytes: st.have_bytes,
            downloaded: self.downloaded.load(Relaxed),
            uploaded: self.uploaded.load(Relaxed),
            down_rate: st.down_rate,
            up_rate: st.up_rate,
            down_limit: self.down_limit.rate(),
            up_limit: self.up_limit.rate(),
            status: st.status.clone(),
            added_on: st.added_on,
            completed_on: st.completed_on,
            known_peers: st.peers.len(),
            pieces,
            files,
            peers,
            trackers,
        }
    }

    fn update_rates(&self) {
        let d = self.downloaded.load(Relaxed);
        let u = self.uploaded.load(Relaxed);
        let mut st = self.lock();
        let dd = d.saturating_sub(st.last_down);
        let du = u.saturating_sub(st.last_up);
        // Smoothed rates that fall to zero quickly once traffic stops.
        st.down_rate = if dd == 0 && st.down_rate < 4096 {
            0
        } else {
            (st.down_rate + dd) / 2
        };
        st.up_rate = if du == 0 && st.up_rate < 4096 {
            0
        } else {
            (st.up_rate + du) / 2
        };
        st.last_down = d;
        st.last_up = u;
        for p in st.connected.values() {
            let (d, u) = (p.down.load(Relaxed), p.up.load(Relaxed));
            let mut last = p.last.lock().unwrap_or_else(|e| e.into_inner());
            let smooth = |rate: u64, delta: u64| {
                if delta == 0 && rate < 4096 {
                    0
                } else {
                    (rate + delta) / 2
                }
            };
            p.down_rate
                .store(smooth(p.down_rate.load(Relaxed), d - last.0), Relaxed);
            p.up_rate
                .store(smooth(p.up_rate.load(Relaxed), u - last.1), Relaxed);
            *last = (d, u);
        }
    }

    /// Hash-check all pieces (multi-threaded). Returns false if interrupted.
    fn check(&self, epoch: u64) -> bool {
        let (meta, storage) = {
            let mut st = self.lock();
            let (Some(m), Some(s)) = (st.meta.clone(), st.storage.clone()) else {
                return true;
            };
            st.status = Status::Checking;
            st.check_progress = 0.0;
            (m, s)
        };
        let n = meta.num_pieces();
        let have: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
        if storage.any_file_exists() {
            let next = AtomicUsize::new(0);
            let threads = std::thread::available_parallelism()
                .map_or(2, |p| p.get())
                .clamp(1, 4);
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| {
                        loop {
                            if !self.active(epoch) {
                                return;
                            }
                            let i = next.fetch_add(1, SeqCst);
                            if i >= n {
                                return;
                            }
                            let ok = storage
                                .hash_piece(i, meta.piece_len(i))
                                .map(|h| h == meta.hashes[i])
                                .unwrap_or(false);
                            have[i].store(ok, Relaxed);
                            if i.is_multiple_of(32) {
                                self.lock().check_progress = i as f32 / n as f32;
                            }
                        }
                    });
                }
            });
        }
        if !self.active(epoch) {
            return false;
        }
        let mut st = self.lock();
        st.have = have.iter().map(|h| h.load(Relaxed)).collect();
        st.have_seq = (0..n as u32).filter(|&i| st.have[i as usize]).collect();
        st.have_count = st.have_seq.len();
        st.have_bytes = st
            .have_seq
            .iter()
            .map(|&i| meta.piece_len(i as usize))
            .sum();
        st.partial.clear();
        st.needs_check = false;
        st.dirty = true;
        recompute_prio(&mut st);
        st.status = if finished(&st) {
            if st.completed_on == 0 {
                st.completed_on = now_unix();
            }
            Status::Seeding
        } else {
            Status::Downloading
        };
        true
    }

    fn connect_peers(self: &Arc<Self>, epoch: u64) {
        let now = Instant::now();
        let mut targets = Vec::new();
        {
            let mut st = self.lock();
            let max_peers = self.ctx.max_peers.load(Relaxed);
            let limit = if finished(&st) {
                max_peers / 2
            } else {
                max_peers
            };
            let cur = st.connected.len() + st.half_open;
            if cur >= limit || self.ctx.conns.load(Relaxed) >= self.ctx.max_conns.load(Relaxed) {
                return;
            }
            let room = (limit - cur)
                .min(MAX_HALF_OPEN.saturating_sub(st.half_open))
                .min(8);
            let st = &mut *st;
            for (addr, p) in st.peers.iter_mut() {
                if targets.len() >= room {
                    break;
                }
                if p.next_try <= now && !st.connected.contains_key(addr) {
                    p.next_try = now + Duration::from_secs(300);
                    targets.push(*addr);
                }
            }
            st.half_open += targets.len();
        }
        for addr in targets {
            let t = self.clone();
            crate::util::spawn(move || conn::outgoing(t, addr, epoch));
        }
    }

    fn announce_numbers(&self) -> (u64, u64, u64) {
        let st = self.lock();
        let left = if st.meta.is_some() {
            st.wanted_bytes - st.have_wanted_bytes
        } else {
            16384
        };
        (
            self.uploaded.load(Relaxed),
            self.downloaded.load(Relaxed),
            left,
        )
    }
}

fn tracker_loop(t: Arc<Torrent>, epoch: u64, url: String) {
    let mut event = Event::Started;
    let mut fails = 0u32;
    let mut was_complete = t.is_finished();
    let still_listed = || t.lock().trackers.contains(&url);
    let set_info = |f: &dyn Fn(&mut TrackerInfo)| {
        let mut st = t.lock();
        if st.trackers.contains(&url) {
            f(st.tracker_info.entry(url.clone()).or_default());
        }
    };
    while t.active(epoch) && still_listed() {
        set_info(&|i| {
            i.status = "Updating\u{2026}".into();
            i.next = None;
        });
        let (uploaded, downloaded, left) = t.announce_numbers();
        let req = Announce {
            info_hash: &t.info_hash,
            peer_id: &t.ctx.peer_id,
            port: t.ctx.port,
            uploaded,
            downloaded,
            left,
            event,
        };
        let wait = match tracker::announce(&url, &req) {
            Ok(r) => {
                fails = 0;
                event = Event::None;
                t.add_peers(&r.peers);
                let wait = r.interval.clamp(120, 3600);
                set_info(&|i| {
                    i.status = "Working".into();
                    i.peers = r.peers.len();
                    i.seeders = r.seeders;
                    i.leechers = r.leechers;
                    i.next = Some(Instant::now() + Duration::from_secs(wait));
                });
                wait
            }
            Err(e) => {
                fails += 1;
                let wait = (30u64 << fails.min(7)).min(3600);
                set_info(&|i| {
                    i.status = e.0.clone();
                    i.next = Some(Instant::now() + Duration::from_secs(wait));
                });
                wait
            }
        };
        let deadline = Instant::now() + Duration::from_secs(wait);
        let re = t.reannounce.load(SeqCst);
        // Wake early to report completion, on a forced reannounce, or when removed.
        let stopped = sleep_until_stop(Duration::from_secs(wait), || {
            !t.active(epoch)
                || (!was_complete && t.is_finished())
                || t.reannounce.load(SeqCst) != re
                || Instant::now() >= deadline
                || !still_listed()
        });
        if stopped && t.active(epoch) && !was_complete && t.is_finished() {
            was_complete = true;
            event = Event::Completed;
        }
    }
    let mut st = t.lock();
    if let Some(i) = st.tracker_info.get_mut(&url) {
        if !t.active(epoch) {
            i.status = String::new();
            i.next = None;
        }
    }
}

fn run(t: Arc<Torrent>, epoch: u64) {
    let mut trackers_started: Vec<String> = Vec::new();
    let mut last_dht: Option<Instant> = None;
    let mut last_save_flag = Instant::now();
    while t.active(epoch) {
        let needs_check = t.lock().needs_check;
        if needs_check && !t.check(epoch) {
            return;
        }
        // Start announce loops for any trackers not yet running.
        let trackers = t.lock().trackers.clone();
        trackers_started.retain(|u| trackers.contains(u));
        for url in trackers {
            if !trackers_started.contains(&url) && trackers_started.len() < 40 {
                trackers_started.push(url.clone());
                let t2 = t.clone();
                crate::util::spawn(move || tracker_loop(t2, epoch, url));
            }
        }
        t.update_rates();
        t.connect_peers(epoch);
        if let Some(dht) = &t.ctx.dht {
            let peers = t.lock().connected.len();
            let every = if peers < 8 { 90 } else { 900 };
            if last_dht.is_none_or(|l| l.elapsed() > Duration::from_secs(every)) {
                last_dht = Some(Instant::now());
                dht.search(t.info_hash, Arc::downgrade(&t));
            }
        }
        if last_save_flag.elapsed() > Duration::from_secs(60) {
            last_save_flag = Instant::now();
            t.lock().dirty = true;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}
