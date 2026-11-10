# rustorrent

A small, fast, portable BitTorrent client for Windows, written in Rust.

* **One file.** `rustorrent.exe` is the whole program (about 0.7 MB). No installer, no runtime, no DLLs beyond what ships with Windows. Double-click it and the window opens.
* **Native GUI.** Plain Win32 controls (list view, buttons, status bar) instead of a GUI framework, which keeps the binary tiny and the UI instant.
* **Few dependencies.** The engine is written from scratch on the Rust standard library; the only crates are `sha1_smol` (hashing) and `windows-sys` (API bindings).

## Features

* `.torrent` files and magnet links (metadata download via BEP 9)
* HTTP, HTTPS (via WinHTTP) and UDP trackers
* DHT (BEP 5) and peer exchange (BEP 11) for trackerless magnets
* Hybrid v1/v2 torrents (connects to peers using either info hash)
* Rarest-first piece picking, request pipelining, end-game mode
* Seeding, with multithreaded hash checking
* Resume after restart, pause/start, force recheck, force reannounce, remove (with or without deleting files)
* **Choose which files to download** when adding a torrent, or later in the Files tab; per-file priority (skip / normal / high)
* **Speed limits**: global download/upload limits and per-torrent limits
* **Automatic port forwarding** with NAT-PMP and UPnP IGD (renewed every 30 minutes, removed on exit)
* **Protocol encryption (MSE/PE)**: prefer, require or disable; compatible with qBittorrent, Deluge, Transmission and other libtorrent-based clients
* **Details panel** with General (pieces bar, sizes, ratio, hashes), Trackers (status, seeds/leechers, add/remove), Peers (client, flags, speeds) and Files tabs
* **Settings dialog**: download folder, listening port, UPnP, DHT, connection limits, speed limits, encryption
* Drag and drop `.torrent` files, paste magnet links with Ctrl+V, copy magnet link
* Single instance: opening a `.torrent` or magnet with the exe hands it to the running window

## Using it

1. Download `rustorrent.exe` (from the build artifacts or a release) and put it in any folder.
2. Double-click it. On first run Windows Firewall may ask to allow incoming connections; allow it for better speeds.
3. Click **Add torrent…**, **Add magnet…**, drag a `.torrent` onto the window, or press Ctrl+V with a magnet link copied.

Downloads go to your `Downloads` folder by default; change it in **Settings…**.
Right-click a torrent for more actions (speed limit, recheck, reannounce). Del removes it, Shift+Del removes it and deletes its files.
Select a torrent to see its details below the list; drag the bar between them to resize. In the Files tab, untick a file (or press Space) to skip it, or right-click for priorities.

Peer flags in the Peers tab follow qBittorrent: `D`/`d` downloading from the peer (unchoked/choked), `U`/`u` uploading to it, `I` incoming connection, `E` encrypted, `e` encrypted handshake only.

Settings and resume data are stored in a `rustorrent_data` folder next to the exe (portable). If that folder cannot be written, `%APPDATA%\rustorrent` is used.

### Bahasa Melayu (ringkas)

Muat turun `rustorrent.exe`, letak di mana-mana folder dan klik dua kali. Tiada installer. Klik **Add torrent…** atau **Add magnet…**, atau seret fail `.torrent` ke tetingkap; anda boleh pilih fail mana yang hendak dimuat turun. Fail disimpan ke folder `Downloads`; tukar dalam **Settings…** (juga untuk had kelajuan, port, UPnP dan enkripsi). Klik kanan torrent untuk had kelajuan per torrent. Data tetapan disimpan dalam folder `rustorrent_data` di sebelah exe.

## Building

```
cargo build --release --target x86_64-pc-windows-msvc
```

The release profile is tuned for size (`opt-level = "z"`, LTO, one codegen unit, stripped) while SHA-1 is compiled at `opt-level = 3` because hashing is the hot path. The C runtime is linked statically (`.cargo/config.toml`), so the exe runs on a clean Windows 10/11 install.

Cross-compiling from Linux also works with `x86_64-pc-windows-gnu` and mingw-w64.

On non-Windows hosts the same engine builds as a headless command-line tool used for development and tests:

```
cargo test
cargo run --release -- --dir ./downloads some.torrent "magnet:?xt=urn:btih:..."
```

## Layout

| File | What it does |
| --- | --- |
| `src/gui/mod.rs` | Win32 main window, torrent list, toolbar, status bar |
| `src/gui/details.rs` | Details panel: General, Trackers, Peers, Files |
| `src/gui/dialogs.rs` | Settings, add torrent, speed limit and text input dialogs |
| `src/session.rs` | Owns torrents, listen socket, DHT, saved state |
| `src/torrent.rs` | Piece picking, verification, peer bookkeeping, main loop |
| `src/conn.rs` | Peer wire protocol, extension protocol, metadata, PEX |
| `src/mse.rs` | Protocol encryption (MSE/PE): DH key exchange, RC4 |
| `src/limit.rs` | Token-bucket speed limits |
| `src/portmap.rs` | NAT-PMP and UPnP port forwarding |
| `src/dht.rs` | Mainline DHT node |
| `src/tracker.rs` | HTTP(S) and UDP announces |
| `src/storage.rs` | Maps pieces to files, positional disk I/O |
| `src/metainfo.rs`, `src/bencode.rs`, `src/magnet.rs` | Parsing |

## Not yet supported

uTP, IPv6 DHT, torrent queueing, and pure v2 torrents.

When some files are skipped, pieces that straddle a skipped and a wanted file are still written, so a skipped file may appear on disk partly filled (Transmission behaves the same way).
