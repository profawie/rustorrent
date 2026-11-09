//! End-to-end tests: two sessions exchanging a torrent over localhost.

use crate::metainfo::create_single;
use crate::session::{Config, Session};
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
