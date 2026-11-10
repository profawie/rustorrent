//! Session: owns all torrents, the listen socket, the DHT and persisted state.

use crate::bail;
use crate::bencode::{self, Value, dict};
use crate::conn;
use crate::dht::Dht;
use crate::magnet;
use crate::metainfo::Metainfo;
use crate::portmap::PortMapper;
use crate::torrent::{Ctx, Details, Stats, Torrent};
use crate::tracker::parse_compact_v4;
use crate::util::{Result, hex, rand_bytes};
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Config {
    pub data_dir: PathBuf,
    /// 0 = pick automatically (6881-6889, then any).
    pub port: u16,
    pub dht: bool,
    pub persist: bool,
    pub download_dir: PathBuf,
}

/// User-adjustable settings (the Settings dialog).
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    /// Listening port; 0 = pick automatically. Takes effect after a restart.
    pub port: u16,
    /// Forward the port on the router with UPnP / NAT-PMP.
    pub upnp: bool,
    /// Takes effect after a restart.
    pub dht: bool,
    /// One of `mse::ENC_*`.
    pub encryption: u8,
    /// Bytes per second, 0 = unlimited.
    pub down_limit: u64,
    pub up_limit: u64,
    pub max_conns: usize,
    pub max_peers: usize,
    pub download_dir: PathBuf,
    /// Show the file selection dialog when adding a torrent (GUI only).
    pub ask_files: bool,
}

/// Options for adding a torrent.
#[derive(Default)]
pub struct AddOptions {
    pub save_dir: Option<PathBuf>,
    pub file_prio: Option<Vec<u8>>,
    pub paused: bool,
}

pub struct Session {
    ctx: Arc<Ctx>,
    torrents: Arc<Mutex<Vec<Arc<Torrent>>>>,
    next_id: AtomicU64,
    data_dir: PathBuf,
    persist: bool,
    pub download_dir: Mutex<PathBuf>,
    save_lock: Mutex<()>,
    settings: Mutex<Settings>,
    portmap: Mutex<Option<Arc<PortMapper>>>,
}

const STATE_FILE: &str = "state.dat";

fn bind_listener(port: u16) -> Option<TcpListener> {
    if port != 0 {
        if let Ok(l) = TcpListener::bind(("0.0.0.0", port)) {
            return Some(l);
        }
    }
    for p in 6881..=6889 {
        if let Ok(l) = TcpListener::bind(("0.0.0.0", p)) {
            return Some(l);
        }
    }
    TcpListener::bind("0.0.0.0:0").ok()
}

fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

impl Session {
    pub fn new(cfg: Config) -> Arc<Session> {
        let _ = fs::create_dir_all(cfg.data_dir.join("torrents"));
        let saved = if cfg.persist {
            fs::read(cfg.data_dir.join(STATE_FILE))
                .ok()
                .and_then(|d| bencode::decode(&d).ok())
        } else {
            None
        };
        let int = |k: &str| saved.as_ref().and_then(|s| s.get(k)).and_then(Value::int);
        let saved_port = int("port").unwrap_or(0) as u16;
        let mut settings = Settings {
            port: int("listen_port").unwrap_or(0).clamp(0, 65535) as u16,
            upnp: int("upnp").unwrap_or(1) != 0,
            dht: int("dht").unwrap_or(1) != 0,
            encryption: int("encryption").unwrap_or(0).clamp(0, 2) as u8,
            down_limit: int("down_limit").unwrap_or(0).max(0) as u64,
            up_limit: int("up_limit").unwrap_or(0).max(0) as u64,
            max_conns: int("max_conns").unwrap_or(500).clamp(10, 5000) as usize,
            max_peers: int("max_peers").unwrap_or(80).clamp(5, 1000) as usize,
            download_dir: cfg.download_dir.clone(),
            ask_files: int("ask_files").unwrap_or(1) != 0,
        };
        if !cfg.persist {
            settings.upnp = false;
        }
        let want_port = if cfg.port != 0 {
            cfg.port
        } else if settings.port != 0 {
            settings.port
        } else {
            saved_port
        };
        let listener = bind_listener(want_port);
        let port = listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map_or(0, |a| a.port());

        let dht = if cfg.dht && settings.dht {
            let id = saved
                .as_ref()
                .and_then(|s| s.get("dht_id"))
                .and_then(Value::bytes)
                .and_then(|b| b.try_into().ok());
            let nodes = saved
                .as_ref()
                .and_then(|s| s.get("dht_nodes"))
                .and_then(Value::bytes)
                .map(parse_compact_v4)
                .unwrap_or_default();
            Dht::start(port, id, nodes)
        } else {
            None
        };
        let mut peer_id = *b"-RT0200-000000000000";
        peer_id[8..].copy_from_slice(&rand_bytes::<12>());
        let ctx = Arc::new(Ctx::new(peer_id, port, dht));
        let download_dir = saved
            .as_ref()
            .and_then(|s| s.get("download_dir"))
            .and_then(Value::str)
            .map(PathBuf::from)
            .unwrap_or(cfg.download_dir);
        settings.download_dir = download_dir.clone();
        let portmap = (settings.upnp && port != 0).then(|| PortMapper::start(port));
        let s = Arc::new(Session {
            ctx,
            torrents: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(1),
            data_dir: cfg.data_dir,
            persist: cfg.persist,
            download_dir: Mutex::new(download_dir),
            save_lock: Mutex::new(()),
            settings: Mutex::new(settings.clone()),
            portmap: Mutex::new(portmap),
        });
        s.apply_live(&settings);
        if let Some(l) = listener {
            let torrents = s.torrents.clone();
            let ctx = s.ctx.clone();
            crate::util::spawn(move || accept_loop(l, torrents, ctx));
        }
        if let Some(saved) = saved {
            s.restore(&saved);
        }
        s
    }

    pub fn port(&self) -> u16 {
        self.ctx.port
    }

    pub fn dht_nodes(&self) -> Option<usize> {
        self.ctx.dht.as_ref().map(|d| d.node_count())
    }

    pub fn settings(&self) -> Settings {
        let mut s = self
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        s.download_dir = self.download_dir.lock().unwrap().clone();
        s
    }

    /// Settings that apply immediately.
    fn apply_live(&self, s: &Settings) {
        self.ctx.down.set_rate(s.down_limit);
        self.ctx.up.set_rate(s.up_limit);
        self.ctx.max_conns.store(s.max_conns, Relaxed);
        self.ctx.max_peers.store(s.max_peers, Relaxed);
        self.ctx.encryption.store(s.encryption, Relaxed);
    }

    /// Apply new settings. Returns true if some of them need a restart.
    pub fn apply_settings(&self, new: Settings) -> bool {
        let old = self.settings();
        self.apply_live(&new);
        *self.download_dir.lock().unwrap() = new.download_dir.clone();
        if new.upnp != old.upnp {
            let mut pm = self.portmap.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = pm.take() {
                p.stop();
            }
            if new.upnp && self.ctx.port != 0 {
                *pm = Some(PortMapper::start(self.ctx.port));
            }
        }
        let restart = (new.port != old.port && new.port != 0 && new.port != self.ctx.port)
            || new.dht != old.dht;
        *self.settings.lock().unwrap_or_else(|e| e.into_inner()) = new;
        self.save();
        restart
    }

    /// Port forwarding state for the status bar.
    pub fn portmap_status(&self) -> Option<String> {
        self.portmap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.status())
    }

    pub fn global_limits(&self) -> (u64, u64) {
        (self.ctx.down.rate(), self.ctx.up.rate())
    }

    pub fn details(&self, id: u64) -> Option<Details> {
        self.get(id).map(|t| t.details())
    }

    /// (download, upload) limit of a torrent in bytes/s; 0 = unlimited.
    pub fn torrent_limits(&self, id: u64) -> Option<(u64, u64)> {
        self.get(id)
            .map(|t| (t.down_limit.rate(), t.up_limit.rate()))
    }

    pub fn set_torrent_limits(&self, id: u64, down: u64, up: u64) {
        if let Some(t) = self.get(id) {
            t.down_limit.set_rate(down);
            t.up_limit.set_rate(up);
            t.lock().dirty = true;
        }
    }

    pub fn set_file_prio(&self, id: u64, files: &[usize], prio: u8) {
        if let Some(t) = self.get(id) {
            t.set_file_prio(files, prio);
        }
    }

    pub fn add_trackers(&self, id: u64, urls: &[String]) {
        if let Some(t) = self.get(id) {
            t.add_trackers(urls);
        }
    }

    pub fn remove_trackers(&self, id: u64, urls: &[String]) {
        if let Some(t) = self.get(id) {
            t.remove_trackers(urls);
        }
    }

    pub fn reannounce(&self, id: u64) {
        if let Some(t) = self.get(id) {
            t.force_reannounce();
            if let Some(d) = &self.ctx.dht {
                d.search(t.info_hash, Arc::downgrade(&t));
            }
        }
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Torrent>>> {
        self.torrents.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: u64) -> Option<Arc<Torrent>> {
        self.list().iter().find(|t| t.id == id).cloned()
    }

    fn info_path(&self, ih: &[u8; 20]) -> PathBuf {
        self.data_dir
            .join("torrents")
            .join(format!("{}.info", hex(ih)))
    }

    fn restore(&self, saved: &Value) {
        for e in saved.get("torrents").and_then(Value::list).unwrap_or(&[]) {
            let _ = (|| -> Option<()> {
                let ih: [u8; 20] = e.get("info_hash")?.bytes()?.try_into().ok()?;
                let save_dir = PathBuf::from(e.get("save_dir")?.str()?);
                let name = e
                    .get("name")
                    .and_then(Value::str)
                    .unwrap_or_else(|| hex(&ih));
                let trackers: Vec<String> = e
                    .get("trackers")
                    .and_then(Value::list)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(Value::str)
                    .collect();
                let meta = fs::read(self.info_path(&ih))
                    .ok()
                    .and_then(|b| Metainfo::from_info(b).ok())
                    .filter(|m| m.info_hash == ih);
                let id = self.next_id.fetch_add(1, SeqCst);
                let t = Torrent::new(
                    id,
                    ih,
                    save_dir,
                    self.ctx.clone(),
                    meta.map(Arc::new),
                    name,
                    trackers,
                );
                let int = |k: &str| e.get(k).and_then(Value::int).unwrap_or(0).max(0) as u64;
                t.uploaded.store(int("uploaded"), Relaxed);
                t.down_limit.set_rate(int("down_limit"));
                t.up_limit.set_rate(int("up_limit"));
                let prio = e
                    .get("file_prio")
                    .and_then(Value::bytes)
                    .map(|b| b.to_vec());
                let mut restored_have = None;
                {
                    let mut st = t.lock();
                    if e.get("added_on").is_some() {
                        st.added_on = int("added_on");
                    }
                    st.completed_on = int("completed_on");
                    if st.meta.is_none() {
                        st.pending_prio = prio;
                    } else if let Some(p) = prio.filter(|p| p.len() == st.file_prio.len()) {
                        st.file_prio = p;
                    }
                    if let (Some(m), Some(storage)) = (st.meta.clone(), st.storage.clone()) {
                        let n = m.num_pieces();
                        let packed = e.get("have").and_then(Value::bytes).unwrap_or(&[]);
                        if packed.len() == n.div_ceil(8) {
                            let have: Vec<bool> = (0..n)
                                .map(|i| packed[i / 8] & (0x80 >> (i % 8)) != 0)
                                .collect();
                            if storage.files_present_for(&have) {
                                restored_have = Some(have);
                            } else {
                                st.needs_check = true;
                            }
                        } else {
                            st.needs_check = storage.any_file_exists();
                        }
                    }
                }
                match restored_have {
                    Some(have) => t.set_have(have),
                    // Recompute wanted counters for the restored file priorities.
                    None => t.set_have(t.lock().have.clone()),
                }
                let paused = e.get("paused").and_then(Value::int).unwrap_or(0) != 0;
                self.list().push(t.clone());
                if !paused {
                    t.start();
                }
                Some(())
            })();
        }
    }

    pub fn add_torrent_file(&self, path: &Path) -> Result<u64> {
        let len = fs::metadata(path)?.len();
        if len > 64 << 20 {
            bail!("file is too large to be a torrent");
        }
        self.add_torrent_bytes(&fs::read(path)?)
    }

    pub fn add_torrent_bytes(&self, data: &[u8]) -> Result<u64> {
        self.add_torrent_with(data, AddOptions::default())
    }

    /// True if a torrent with this info hash is already in the list.
    pub fn contains(&self, info_hash: &[u8; 20]) -> bool {
        self.list().iter().any(|t| &t.info_hash == info_hash)
    }

    pub fn add_torrent_with(&self, data: &[u8], opts: AddOptions) -> Result<u64> {
        let (meta, trackers) = Metainfo::from_torrent(data)?;
        if let Some(t) = self.list().iter().find(|t| t.info_hash == meta.info_hash) {
            t.add_trackers(&trackers);
            return Ok(t.id);
        }
        if self.persist {
            let _ = fs::write(self.info_path(&meta.info_hash), &meta.info_bytes);
        }
        let id = self.next_id.fetch_add(1, SeqCst);
        let dir = opts
            .save_dir
            .unwrap_or_else(|| self.download_dir.lock().unwrap().clone());
        let ih = meta.info_hash;
        let name = meta.name.clone();
        let t = Torrent::new(
            id,
            ih,
            dir,
            self.ctx.clone(),
            Some(Arc::new(meta)),
            name,
            trackers,
        );
        if let Some(p) = opts.file_prio {
            let files: Vec<usize> = (0..p.len()).collect();
            for prio in [0, 1, 2] {
                let sel: Vec<usize> = files.iter().copied().filter(|&i| p[i] == prio).collect();
                if !sel.is_empty() {
                    t.set_file_prio(&sel, prio);
                }
            }
        }
        {
            let mut st = t.lock();
            st.needs_check = st.storage.as_ref().is_some_and(|s| s.any_file_exists());
        }
        self.list().push(t.clone());
        if !opts.paused {
            t.start();
        }
        self.save();
        Ok(id)
    }

    pub fn add_magnet(&self, uri: &str) -> Result<u64> {
        let m = magnet::parse(uri)?;
        if let Some(t) = self.list().iter().find(|t| t.info_hash == m.info_hash) {
            t.add_trackers(&m.trackers);
            return Ok(t.id);
        }
        let id = self.next_id.fetch_add(1, SeqCst);
        let dir = self.download_dir.lock().unwrap().clone();
        let name = m
            .name
            .clone()
            .map(|n| crate::metainfo::sanitize(&n))
            .unwrap_or_else(|| hex(&m.info_hash));
        let t = Torrent::new(
            id,
            m.info_hash,
            dir,
            self.ctx.clone(),
            None,
            name,
            m.trackers,
        );
        self.list().push(t.clone());
        t.start();
        self.save();
        Ok(id)
    }

    #[cfg(any(test, not(windows)))]
    pub fn add_peer(&self, id: u64, addr: SocketAddr) {
        if let Some(t) = self.get(id) {
            t.add_peers(&[addr]);
        }
    }

    pub fn pause(&self, id: u64) {
        if let Some(t) = self.get(id) {
            t.pause();
        }
    }

    pub fn resume(&self, id: u64) {
        if let Some(t) = self.get(id) {
            t.start();
        }
    }

    pub fn recheck(&self, id: u64) {
        if let Some(t) = self.get(id) {
            let mut st = t.lock();
            if st.meta.is_some() {
                st.needs_check = true;
            }
            drop(st);
            if t.is_paused() {
                t.start();
            }
        }
    }

    pub fn remove(&self, id: u64, delete_files: bool) {
        let t = {
            let mut list = self.list();
            let Some(pos) = list.iter().position(|t| t.id == id) else {
                return;
            };
            list.remove(pos)
        };
        t.pause();
        let _ = fs::remove_file(self.info_path(&t.info_hash));
        let storage = t.lock().storage.clone();
        if let Some(s) = storage {
            crate::util::spawn(move || {
                // Give peer threads a moment to notice the stop.
                std::thread::sleep(Duration::from_millis(1500));
                if delete_files {
                    s.delete_files();
                } else {
                    s.close();
                }
            });
        }
        self.save();
    }

    pub fn stats(&self) -> Vec<Stats> {
        self.list().iter().map(|t| t.stats()).collect()
    }

    pub fn magnet_link(&self, id: u64) -> Option<String> {
        let t = self.get(id)?;
        let st = t.lock();
        Some(magnet::build(&t.info_hash, &st.name, &st.trackers))
    }

    /// Path of the downloaded file or folder.
    pub fn content_path(&self, id: u64) -> Option<PathBuf> {
        let t = self.get(id)?;
        let st = t.lock();
        Some(t.save_dir.join(&st.name))
    }

    pub fn any_dirty(&self) -> bool {
        self.list().iter().any(|t| t.lock().dirty)
    }

    pub fn save(&self) {
        if !self.persist {
            return;
        }
        let _guard = self.save_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = Vec::new();
        for t in self.list().iter() {
            let mut st = t.lock();
            st.dirty = false;
            entries.push(dict(vec![
                (
                    "have",
                    Value::Bytes(if st.needs_check {
                        Vec::new()
                    } else {
                        pack_bits(&st.have)
                    }),
                ),
                ("added_on", Value::Int(st.added_on as i64)),
                ("completed_on", Value::Int(st.completed_on as i64)),
                ("down_limit", Value::Int(t.down_limit.rate() as i64)),
                (
                    "file_prio",
                    Value::Bytes(
                        st.pending_prio
                            .clone()
                            .unwrap_or_else(|| st.file_prio.clone()),
                    ),
                ),
                ("info_hash", Value::b(&t.info_hash)),
                ("name", Value::s(&st.name)),
                ("paused", Value::Int(t.is_paused() as i64)),
                ("save_dir", Value::s(&t.save_dir.to_string_lossy())),
                (
                    "trackers",
                    Value::List(st.trackers.iter().map(|s| Value::s(s)).collect()),
                ),
                ("up_limit", Value::Int(t.up_limit.rate() as i64)),
                ("uploaded", Value::Int(t.uploaded.load(Relaxed) as i64)),
            ]));
            // Write metadata fetched from a magnet link once.
            if let Some(m) = &st.meta {
                let p = self.info_path(&t.info_hash);
                if !p.exists() {
                    let _ = fs::write(p, &m.info_bytes);
                }
            }
        }
        let cfg = self.settings();
        let mut top = vec![
            ("ask_files", Value::Int(cfg.ask_files as i64)),
            ("dht", Value::Int(cfg.dht as i64)),
            ("down_limit", Value::Int(cfg.down_limit as i64)),
            (
                "download_dir",
                Value::s(&cfg.download_dir.to_string_lossy()),
            ),
            ("encryption", Value::Int(cfg.encryption as i64)),
            ("listen_port", Value::Int(cfg.port as i64)),
            ("max_conns", Value::Int(cfg.max_conns as i64)),
            ("max_peers", Value::Int(cfg.max_peers as i64)),
            ("port", Value::Int(self.ctx.port as i64)),
            ("torrents", Value::List(entries)),
            ("up_limit", Value::Int(cfg.up_limit as i64)),
            ("upnp", Value::Int(cfg.upnp as i64)),
        ];
        if let Some(d) = &self.ctx.dht {
            top.push(("dht_id", Value::b(&d.id())));
            let mut nodes = Vec::new();
            for a in d.export_nodes() {
                if let SocketAddr::V4(v4) = a {
                    nodes.extend_from_slice(&v4.ip().octets());
                    nodes.extend_from_slice(&v4.port().to_be_bytes());
                }
            }
            top.push(("dht_nodes", Value::Bytes(nodes)));
        }
        let data = dict(top).encode();
        let tmp = self.data_dir.join("state.tmp");
        if fs::write(&tmp, data).is_ok() {
            let _ = fs::rename(&tmp, self.data_dir.join(STATE_FILE));
        }
    }

    pub fn shutdown(&self) {
        self.save();
        for t in self.list().iter() {
            t.pause();
        }
        if let Some(p) = self
            .portmap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            p.stop();
        }
    }
}

fn accept_loop(l: TcpListener, torrents: Arc<Mutex<Vec<Arc<Torrent>>>>, ctx: Arc<Ctx>) {
    for stream in l.incoming() {
        let Ok(s) = stream else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let torrents = torrents.clone();
        let ctx = ctx.clone();
        crate::util::spawn(move || {
            let list = || torrents.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let r = conn::accept(
                s,
                &ctx,
                |ih| list().into_iter().find(|t| t.matches_hash(ih)),
                || {
                    let mut out = Vec::new();
                    for t in list() {
                        out.push(t.info_hash);
                        if let Some(v2) = t.meta().and_then(|m| m.info_hash_v2) {
                            out.push(v2);
                        }
                    }
                    out
                },
            );
            if std::env::var_os("RT_DEBUG").is_some() {
                eprintln!("incoming ended: {:?}", r.err());
            }
        });
    }
}
