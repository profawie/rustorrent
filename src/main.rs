#![cfg_attr(windows, windows_subsystem = "windows")]
// The headless (non-Windows) build only exercises part of the API.
#![cfg_attr(not(windows), allow(dead_code))]
#![allow(clippy::collapsible_if)]

mod bencode;
mod conn;
mod dht;
mod limit;
mod magnet;
mod metainfo;
mod mse;
mod portmap;
mod session;
mod storage;
mod torrent;
mod tracker;
mod util;

#[cfg(windows)]
mod gui;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

/// Portable data folder: next to the executable if writable, else the user's app-data folder.
pub fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(find_data_dir).clone()
}

fn find_data_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rustorrent_data")))
    {
        if std::fs::create_dir_all(&dir).is_ok() {
            let probe = dir.join(".probe");
            if std::fs::write(&probe, b"ok").is_ok() {
                let _ = std::fs::remove_file(probe);
                return dir;
            }
        }
    }
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    base.join("rustorrent")
}

/// Append a timestamped line to `rustorrent.log` in the data folder.
/// The GUI build has no console, so this is the only trace of startup problems.
pub fn log(msg: &str) {
    use std::io::Write;
    let path = data_dir().join("rustorrent.log");
    // Keep the log small: start over once it passes 256 KB.
    let append = std::fs::metadata(&path).is_ok_and(|m| m.len() < 256 * 1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&path);
    if let Ok(mut f) = file {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let _ = writeln!(
            f,
            "[{}.{:03}] pid {} {}",
            secs / 1000,
            secs % 1000,
            std::process::id(),
            msg
        );
    }
}

pub fn default_download_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(windows)]
fn main() {
    gui::run();
}

/// Headless mode for non-Windows hosts (used for development and testing).
#[cfg(not(windows))]
fn main() {
    use session::{Config, Session};
    let mut args = std::env::args().skip(1);
    let mut cfg = Config {
        data_dir: data_dir(),
        port: 0,
        dht: true,
        persist: false,
        download_dir: PathBuf::from("."),
    };
    let mut items = Vec::new();
    let mut peers = Vec::new();
    let mut exit_on_complete = false;
    let mut enc = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => cfg.port = args.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            "--dir" => cfg.download_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--no-dht" => cfg.dht = false,
            "--peer" => peers.extend(
                args.next()
                    .and_then(|p| p.parse::<std::net::SocketAddr>().ok()),
            ),
            "--exit-on-complete" => exit_on_complete = true,
            "--enc" => {
                enc = match args.next().as_deref() {
                    Some("require") => Some(mse::ENC_REQUIRE),
                    Some("disable") => Some(mse::ENC_DISABLE),
                    _ => Some(mse::ENC_PREFER),
                }
            }
            "--make" => {
                let path = PathBuf::from(args.next().expect("--make <file>"));
                let data = metainfo::create_single(&path, 256 * 1024, None).expect("create");
                let out = path.with_extension("torrent");
                std::fs::write(&out, data).expect("write");
                println!("wrote {}", out.display());
                return;
            }
            _ => items.push(a),
        }
    }
    let s = Session::new(cfg);
    if let Some(e) = enc {
        let mut st = s.settings();
        st.encryption = e;
        s.apply_settings(st);
    }
    let mut ids = Vec::new();
    for it in items {
        let r = if it.starts_with("magnet:") {
            s.add_magnet(&it)
        } else {
            s.add_torrent_file(std::path::Path::new(&it))
        };
        match r {
            Ok(id) => ids.push(id),
            Err(e) => eprintln!("{}: {}", it, e),
        }
    }
    for id in &ids {
        for p in &peers {
            s.add_peer(*id, *p);
        }
    }
    println!("listening on port {}", s.port());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let stats = s.stats();
        for t in &stats {
            println!(
                "{:<40} {:>6.2}% {:?} down {} up {} peers {}/{} dht {:?}",
                t.name,
                t.progress * 100.0,
                t.status,
                util::fmt_rate(t.down_rate),
                util::fmt_rate(t.up_rate),
                t.peers,
                t.known,
                s.dht_nodes()
            );
        }
        if exit_on_complete
            && !stats.is_empty()
            && stats.iter().all(|t| t.status == torrent::Status::Seeding)
        {
            break;
        }
    }
}
