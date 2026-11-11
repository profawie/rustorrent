//! End-to-end tests: two sessions exchanging a torrent over localhost.

use crate::bencode::{Value, dict};
use crate::metainfo::create_single;
use crate::session::{AddOptions, Config, Session};
use crate::torrent::Status;
use crate::util::{hex, rand_u64};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rt-{}-{}", name, rand_u64()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn session(dir: &std::path::Path) -> Arc<Session> {
    Session::new(Config {
        data_dir: dir.join("data"),
        port: 0,
        dht: false,
        persist: true,
        download_dir: dir.to_path_buf(),
    })
}

fn wait_seeding(s: &Session, id: u64, secs: u64) {
    let start = Instant::now();
    loop {
        let st = s.stats().into_iter().find(|t| t.id == id).unwrap();
        if st.status == Status::Seeding {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out: {:?} {:.2}",
            st.status,
            st.progress
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn transfer_torrent_and_magnet() {
    let seed_dir = tmp("seed");
    let data: Vec<u8> = (0..3_000_123u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let file = seed_dir.join("payload.bin");
    std::fs::write(&file, &data).unwrap();
    let torrent = create_single(&file, 32 * 1024, None).unwrap();

    let seeder = session(&seed_dir);
    let sid = seeder.add_torrent_bytes(&torrent).unwrap();
    wait_seeding(&seeder, sid, 30);
    let seed_addr = format!("127.0.0.1:{}", seeder.port()).parse().unwrap();

    // Leecher with the .torrent
    let d1 = tmp("leech1");
    let l1 = session(&d1);
    let id1 = l1.add_torrent_bytes(&torrent).unwrap();
    l1.add_peer(id1, seed_addr);
    wait_seeding(&l1, id1, 60);
    assert!(std::fs::read(d1.join("payload.bin")).unwrap() == data);

    // Leecher with only a magnet link (metadata exchange)
    let (meta, _) = crate::metainfo::Metainfo::from_torrent(&torrent).unwrap();
    let d2 = tmp("leech2");
    let l2 = session(&d2);
    let id2 = l2
        .add_magnet(&format!("magnet:?xt=urn:btih:{}", hex(&meta.info_hash)))
        .unwrap();
    l2.add_peer(id2, seed_addr);
    wait_seeding(&l2, id2, 60);
    assert!(std::fs::read(d2.join("payload.bin")).unwrap() == data);

    // Resume: restart a session from saved state; must come back as seeding without recheck.
    l1.shutdown();
    drop(l1);
    std::thread::sleep(Duration::from_millis(300));
    let l1b = session(&d1);
    let st = l1b.stats();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].progress, 1.0);

    // Remove with delete.
    l2.remove(id2, true);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!d2.join("payload.bin").exists());

    for d in [seed_dir, d1, d2] {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

/// Multi-file torrent from (name, data) pairs, stored under `dir/root`.
fn make_multi(
    dir: &std::path::Path,
    root: &str,
    files: &[(&str, Vec<u8>)],
    plen: usize,
) -> Vec<u8> {
    let mut all = Vec::new();
    let mut list = Vec::new();
    for (name, data) in files {
        let p = dir.join(root).join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, data).unwrap();
        all.extend_from_slice(data);
        list.push(dict(vec![
            ("length", Value::Int(data.len() as i64)),
            ("path", Value::List(vec![Value::s(name)])),
        ]));
    }
    let mut pieces = Vec::new();
    for c in all.chunks(plen) {
        pieces.extend_from_slice(&crate::util::sha1(c));
    }
    let info = dict(vec![
        ("files", Value::List(list)),
        ("name", Value::s(root)),
        ("piece length", Value::Int(plen as i64)),
        ("pieces", Value::Bytes(pieces)),
    ]);
    dict(vec![("info", info)]).encode()
}

fn seed(dir: &std::path::Path, torrent: &[u8]) -> (Arc<Session>, std::net::SocketAddr) {
    let s = session(dir);
    let id = s.add_torrent_bytes(torrent).unwrap();
    wait_seeding(&s, id, 30);
    let addr = format!("127.0.0.1:{}", s.port()).parse().unwrap();
    (s, addr)
}

#[test]
fn file_selection() {
    let seed_dir = tmp("fsseed");
    let files = [
        ("a.bin", pattern(300_000, 1)),
        ("b.bin", pattern(500_000, 2)),
        ("c.bin", pattern(200_000, 3)),
    ];
    let torrent = make_multi(&seed_dir, "multi", &files, 64 * 1024);
    let (_seeder, addr) = seed(&seed_dir, &torrent);

    // Skip b.bin from the start, make c.bin high priority.
    let d = tmp("fsleech");
    let l = session(&d);
    let id = l
        .add_torrent_with(
            &torrent,
            AddOptions {
                file_prio: Some(vec![1, 0, 2]),
                ..Default::default()
            },
        )
        .unwrap();
    l.add_peer(id, addr);
    wait_seeding(&l, id, 60);
    let st = l.stats().into_iter().find(|t| t.id == id).unwrap();
    assert_eq!(st.size, 500_000);
    assert_eq!(st.progress, 1.0);
    assert_eq!(std::fs::read(d.join("multi/a.bin")).unwrap(), files[0].1);
    assert_eq!(std::fs::read(d.join("multi/c.bin")).unwrap(), files[2].1);
    let det = l.details(id).unwrap();
    assert_eq!(det.files[1].prio, 0);
    assert!(
        det.files[1].done < 500_000,
        "skipped file should be mostly missing"
    );
    assert_eq!(det.files[0].done, 300_000);

    // Select b.bin again: the torrent goes back to downloading and completes.
    l.set_file_prio(id, &[1], 1);
    let st = l.stats().into_iter().find(|t| t.id == id).unwrap();
    assert_eq!(st.status, Status::Downloading);
    wait_seeding(&l, id, 60);
    assert_eq!(std::fs::read(d.join("multi/b.bin")).unwrap(), files[1].1);

    // Priorities survive a restart.
    l.set_file_prio(id, &[0], 0);
    l.shutdown();
    drop(l);
    std::thread::sleep(Duration::from_millis(300));
    let l2 = session(&d);
    let det = l2.details(l2.stats()[0].id).unwrap();
    assert_eq!(
        det.files.iter().map(|f| f.prio).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    for dd in [seed_dir, d] {
        let _ = std::fs::remove_dir_all(dd);
    }
}

#[test]
fn speed_limits_and_required_encryption() {
    let seed_dir = tmp("limseed");
    let data = pattern(1_200_000, 9);
    let file = seed_dir.join("lim.bin");
    std::fs::write(&file, &data).unwrap();
    let torrent = create_single(&file, 32 * 1024, None).unwrap();
    let (seeder, addr) = seed(&seed_dir, &torrent);
    let mut cfg = seeder.settings();
    cfg.encryption = crate::mse::ENC_REQUIRE;
    seeder.apply_settings(cfg);

    // Leecher limited to 300 KB/s download, encryption required on both ends.
    let d = tmp("limleech");
    let l = session(&d);
    let mut cfg = l.settings();
    cfg.encryption = crate::mse::ENC_REQUIRE;
    cfg.down_limit = 300_000;
    l.apply_settings(cfg);
    let id = l.add_torrent_bytes(&torrent).unwrap();
    l.add_peer(id, addr);
    let start = Instant::now();
    wait_seeding(&l, id, 60);
    let secs = start.elapsed().as_secs_f64();
    assert!(secs > 2.5, "download limit not applied: {:.1}s", secs);
    assert_eq!(std::fs::read(d.join("lim.bin")).unwrap(), data);

    // Per-torrent upload limit on the seeder side.
    let sid = seeder.stats()[0].id;
    seeder.set_torrent_limits(sid, 0, 400_000);
    let d2 = tmp("limleech2");
    let l2 = session(&d2);
    let id2 = l2.add_torrent_bytes(&torrent).unwrap();
    l2.add_peer(id2, addr);
    let start = Instant::now();
    wait_seeding(&l2, id2, 60);
    let secs = start.elapsed().as_secs_f64();
    assert!(secs > 2.0, "upload limit not applied: {:.1}s", secs);
    for dd in [seed_dir, d, d2] {
        let _ = std::fs::remove_dir_all(dd);
    }
}

#[test]
#[ignore]
fn print_info_hash() {
    let d = std::fs::read(std::env::var("TORRENT").unwrap()).unwrap();
    let (m, _) = crate::metainfo::Metainfo::from_torrent(&d).unwrap();
    println!(
        "ours {} pieces {} total {}",
        hex(&m.info_hash),
        m.num_pieces(),
        m.total_length
    );
}
