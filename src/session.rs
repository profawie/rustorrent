//! Session: owns all torrents, the listen socket, the DHT and persisted state.

use crate::bail;
use crate::bencode::{self, Value, dict};
use crate::conn;
use crate::dht::Dht;
use crate::magnet;
use crate::metainfo::Metainfo;
use crate::torrent::{Ctx, Stats, Torrent};
use crate::tracker::parse_compact_v4;
use crate::util::{Result, hex, rand_bytes};
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
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

pub struct Session {
    ctx: Arc<Ctx>,
    torrents: Arc<Mutex<Vec<Arc<Torrent>>>>,
    next_id: AtomicU64,
    data_dir: PathBuf,
    persist: bool,
    pub download_dir: Mutex<PathBuf>,
    save_lock: Mutex<()>,
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
        let saved_port = saved
            .as_ref()
            .and_then(|s| s.get("port"))
            .and_then(Value::int)
            .unwrap_or(0) as u16;
        let listener = bind_listener(if cfg.port != 0 { cfg.port } else { saved_port });
        let port = listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map_or(0, |a| a.port());

        let dht = if cfg.dht {
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
        let mut peer_id = *b"-RT0100-000000000000";
        peer_id[8..].copy_from_slice(&rand_bytes::<12>());
        let ctx = Arc::new(Ctx {
            peer_id,
            port,
            dht,
            conns: AtomicUsize::new(0),
            max_conns: 400,
        });
        let download_dir = saved
            .as_ref()
            .and_then(|s| s.get("download_dir"))
            .and_then(Value::str)
            .map(PathBuf::from)
            .unwrap_or(cfg.download_dir);
        let s = Arc::new(Session {
            ctx,
            torrents: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(1),
            data_dir: cfg.data_dir,
            persist: cfg.persist,
            download_dir: Mutex::new(download_dir),
            save_lock: Mutex::new(()),
        });
        if let Some(l) = listener {
            let torrents = s.torrents.clone();
            crate::util::spawn(move || accept_loop(l, torrents));
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
                t.uploaded.store(
                    e.get("uploaded").and_then(Value::int).unwrap_or(0).max(0) as u64,
                    Relaxed,
                );
                {
                    let mut st = t.lock();
                    if let (Some(m), Some(storage)) = (st.meta.clone(), st.storage.clone()) {
                        let n = m.num_pieces();
                        let packed = e.get("have").and_then(Value::bytes).unwrap_or(&[]);
                        if packed.len() == n.div_ceil(8) {
                            let have: Vec<bool> = (0..n)
                                .map(|i| packed[i / 8] & (0x80 >> (i % 8)) != 0)
                                .collect();
                            if storage.files_present_for(&have) {
                                st.have_seq = (0..n as u32).filter(|&i| have[i as usize]).collect();
                                st.have_count = st.have_seq.len();
                                st.have_bytes =
                                    st.have_seq.iter().map(|&i| m.piece_len(i as usize)).sum();
                                st.have = have;
                            } else {
                                st.needs_check = true;
                            }
                        } else {
                            st.needs_check = storage.any_file_exists();
                        }
                    }
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
        let (meta, trackers) = Metainfo::from_torrent(data)?;
        if let Some(t) = self.list().iter().find(|t| t.info_hash == meta.info_hash) {
            t.add_trackers(&trackers);
            return Ok(t.id);
        }
        if self.persist {
            let _ = fs::write(self.info_path(&meta.info_hash), &meta.info_bytes);
        }
        let id = self.next_id.fetch_add(1, SeqCst);
        let dir = self.download_dir.lock().unwrap().clone();
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
        {
            let mut st = t.lock();
            st.needs_check = st.storage.as_ref().is_some_and(|s| s.any_file_exists());
        }
        self.list().push(t.clone());
        t.start();
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
                ("info_hash", Value::b(&t.info_hash)),
                ("name", Value::s(&st.name)),
                ("paused", Value::Int(t.is_paused() as i64)),
                ("save_dir", Value::s(&t.save_dir.to_string_lossy())),
                (
                    "trackers",
                    Value::List(st.trackers.iter().map(|s| Value::s(s)).collect()),
                ),
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
        let mut top = vec![
            (
                "download_dir",
                Value::s(&self.download_dir.lock().unwrap().to_string_lossy()),
            ),
            ("port", Value::Int(self.ctx.port as i64)),
            ("torrents", Value::List(entries)),
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
    }
}

fn accept_loop(l: TcpListener, torrents: Arc<Mutex<Vec<Arc<Torrent>>>>) {
    for stream in l.incoming() {
        let Ok(mut s) = stream else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let torrents = torrents.clone();
        crate::util::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
            let Ok(addr) = s.peer_addr() else { return };
            let Ok(hs) = conn::read_handshake(&mut s) else {
                return;
            };
            let t = torrents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|t| t.matches_hash(&hs.info_hash))
                .cloned();
            if let Some(t) = t {
                if !t.is_paused() {
                    conn::incoming(t, s, addr, hs);
                }
            }
        });
    }
}
