# rustorrent

A small, fast, portable BitTorrent client for Windows, written in Rust.

* **One file.** `rustorrent.exe` is the whole program (about 0.6 MB). No installer, no runtime, no DLLs beyond what ships with Windows. Double-click it and the window opens.
* **Native GUI.** Plain Win32 controls (list view, buttons, status bar) instead of a GUI framework, which keeps the binary tiny and the UI instant.
* **Few dependencies.** The engine is written from scratch on the Rust standard library; the only crates are `sha1_smol` (hashing) and `windows-sys` (API bindings).

## Features

* `.torrent` files and magnet links (metadata download via BEP 9)
* HTTP, HTTPS (via WinHTTP) and UDP trackers
* DHT (BEP 5) and peer exchange (BEP 11) for trackerless magnets
* Hybrid v1/v2 torrents (connects to peers using either info hash)
* Rarest-first piece picking, request pipelining, end-game mode
* Seeding, with multithreaded hash checking
* Resume after restart, pause/start, force recheck, remove (with or without deleting files)
* Drag and drop `.torrent` files, paste magnet links with Ctrl+V, copy magnet link
* Single instance: opening a `.torrent` or magnet with the exe hands it to the running window

## Using it

1. Download `rustorrent.exe` (from the build artifacts or a release) and put it in any folder.
2. Double-click it. On first run Windows Firewall may ask to allow incoming connections; allow it for better speeds.
3. Click **Add torrent…**, **Add magnet…**, drag a `.torrent` onto the window, or press Ctrl+V with a magnet link copied.

Downloads go to your `Downloads` folder by default; change it with **Download folder…**.
Right-click a torrent for more actions. Del removes it, Shift+Del removes it and deletes its files.

Settings and resume data are stored in a `rustorrent_data` folder next to the exe (portable). If that folder cannot be written, `%APPDATA%\rustorrent` is used.

### Bahasa Melayu (ringkas)

Muat turun `rustorrent.exe`, letak di mana-mana folder dan klik dua kali. Tiada installer. Klik **Add torrent…** atau **Add magnet…**, atau seret fail `.torrent` ke tetingkap. Fail disimpan ke folder `Downloads`; tukar dengan **Download folder…**. Data tetapan disimpan dalam folder `rustorrent_data` di sebelah exe.

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
| `src/gui.rs` | Win32 window, list view, dialogs |
| `src/session.rs` | Owns torrents, listen socket, DHT, saved state |
| `src/torrent.rs` | Piece picking, verification, peer bookkeeping, main loop |
| `src/conn.rs` | Peer wire protocol, extension protocol, metadata, PEX |
| `src/dht.rs` | Mainline DHT node |
| `src/tracker.rs` | HTTP(S) and UDP announces |
| `src/storage.rs` | Maps pieces to files, positional disk I/O |
| `src/metainfo.rs`, `src/bencode.rs`, `src/magnet.rs` | Parsing |

## Not yet supported

uTP, protocol encryption (MSE/PE), UPnP/NAT-PMP port mapping, IPv6 DHT, speed limits, selective file download, and pure v2 torrents.
