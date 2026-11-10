//! Details panel below the torrent list: General (with a pieces bar), Trackers, Peers and Files.

use super::{dialogs, draw_bar, message, rgb, scale, selected_rows, set_clipboard, w, with_app};
use crate::torrent::{Details, PRIO_HIGH, PRIO_NORMAL, PRIO_SKIP, Status};
use crate::util::{fmt_bytes, fmt_eta, fmt_rate, hex};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Time::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub(super) const IDC_TAB: usize = 110;
pub(super) const IDC_GENERAL: usize = 111;
pub(super) const IDC_TRACKERS: usize = 112;
pub(super) const IDC_PEERS: usize = 113;
pub(super) const IDC_FILES: usize = 114;
pub(super) const GENERAL_CLASS: &str = "RustorrentGeneral";

pub(super) const ID_FILE_NORMAL: usize = 301;
pub(super) const ID_FILE_HIGH: usize = 302;
pub(super) const ID_FILE_SKIP: usize = 303;
pub(super) const ID_FILE_OPEN: usize = 304;
pub(super) const ID_FILE_FOLDER: usize = 305;
pub(super) const ID_TRACKER_ADD: usize = 311;
pub(super) const ID_TRACKER_REMOVE: usize = 312;
pub(super) const ID_TRACKER_COPY: usize = 313;
pub(super) const ID_TRACKER_REANNOUNCE: usize = 314;
pub(super) const ID_PEER_COPY: usize = 321;

const TABS: [&str; 4] = ["General", "Trackers", "Peers", "Files"];
type Cols = &'static [(&'static str, i32, bool)];
const TRACKER_COLS: Cols = &[
    ("URL", 380, false),
    ("Status", 220, false),
    ("Peers", 60, true),
    ("Seeds", 60, true),
    ("Leechers", 70, true),
    ("Next announce", 100, true),
];
const PEER_COLS: Cols = &[
    ("Address", 200, false),
    ("Client", 170, false),
    ("Flags", 60, false),
    ("Progress", 70, true),
    ("Down", 85, true),
    ("Up", 85, true),
    ("Downloaded", 90, true),
    ("Uploaded", 90, true),
];
const FILE_COLS: Cols = &[
    ("Name", 440, false),
    ("Size", 85, true),
    ("Progress", 120, false),
    ("Priority", 100, false),
];
pub(super) const FILE_PROGRESS_COL: i32 = 2;

pub(super) struct FileRow {
    pub index: usize,
    pub prio: u8,
    pub progress: f64,
    pub rel: PathBuf,
}

pub(super) struct Panel {
    pub tab: HWND,
    pub general: HWND,
    /// Trackers, peers, files.
    pub lists: [HWND; 3],
    pub cur: usize,
    pub rows: [Vec<Vec<String>>; 3],
    pub files: Vec<FileRow>,
    pub tracker_urls: Vec<String>,
    pub snap: Option<Details>,
}

impl Panel {
    pub fn list_index(&self, h: HWND) -> Option<usize> {
        self.lists.iter().position(|&l| l == h)
    }
}

pub(super) unsafe fn create_list(
    parent: HWND,
    hinst: HINSTANCE,
    id: usize,
    font: WPARAM,
    cols: Cols,
    dpi: i32,
    checkboxes: bool,
) -> HWND {
    unsafe {
        let class = w("SysListView32");
        let list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            class.as_ptr(),
            null(),
            WS_CHILD | WS_TABSTOP | LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS,
            0,
            0,
            10,
            10,
            parent,
            id as HMENU,
            hinst,
            null(),
        );
        SendMessageW(list, WM_SETFONT, font, 1);
        let mut ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        if checkboxes {
            ex |= LVS_EX_CHECKBOXES;
        }
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            ex as WPARAM,
            ex as LPARAM,
        );
        if checkboxes {
            // Owner-data list: we supply the check state ourselves.
            SendMessageW(list, LVM_SETCALLBACKMASK, LVIS_STATEIMAGEMASK as WPARAM, 0);
        }
        for (i, (name, width, right)) in cols.iter().enumerate() {
            let mut text = w(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            col.cx = scale(*width, dpi);
            col.pszText = text.as_mut_ptr();
            col.iSubItem = i as i32;
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
        }
        list
    }
}

pub(super) unsafe fn create(parent: HWND, hinst: HINSTANCE, font: WPARAM, dpi: i32) -> Panel {
    unsafe {
        let tab = CreateWindowExW(
            0,
            WC_TABCONTROLW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_TABSTOP | TCS_FOCUSNEVER,
            0,
            0,
            10,
            10,
            parent,
            IDC_TAB as HMENU,
            hinst,
            null(),
        );
        SendMessageW(tab, WM_SETFONT, font, 1);
        for (i, name) in TABS.iter().enumerate() {
            let mut t = w(name);
            let mut item: TCITEMW = std::mem::zeroed();
            item.mask = TCIF_TEXT;
            item.pszText = t.as_mut_ptr();
            SendMessageW(tab, TCM_INSERTITEMW, i, &item as *const _ as LPARAM);
        }
        let gclass = w(GENERAL_CLASS);
        let general = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            gclass.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            10,
            10,
            parent,
            IDC_GENERAL as HMENU,
            hinst,
            null(),
        );
        let lists = [
            create_list(parent, hinst, IDC_TRACKERS, font, TRACKER_COLS, dpi, false),
            create_list(parent, hinst, IDC_PEERS, font, PEER_COLS, dpi, false),
            create_list(parent, hinst, IDC_FILES, font, FILE_COLS, dpi, true),
        ];
        Panel {
            tab,
            general,
            lists,
            cur: 0,
            rows: Default::default(),
            files: Vec::new(),
            tracker_urls: Vec::new(),
            snap: None,
        }
    }
}

/// Position the tab control and the page inside it.
pub(super) unsafe fn layout(x: i32, y: i32, width: i32, height: i32) {
    let Some((tab, general, lists, cur)) =
        with_app(|a| (a.panel.tab, a.panel.general, a.panel.lists, a.panel.cur))
    else {
        return;
    };
    unsafe {
        MoveWindow(tab, x, y, width, height, 1);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        SendMessageW(tab, TCM_ADJUSTRECT, 0, &mut rc as *mut _ as LPARAM);
        let pages = [general, lists[0], lists[1], lists[2]];
        for (i, p) in pages.iter().enumerate() {
            MoveWindow(
                *p,
                x + rc.left,
                y + rc.top,
                (rc.right - rc.left).max(0),
                (rc.bottom - rc.top).max(0),
                1,
            );
            ShowWindow(*p, if i == cur { SW_SHOW } else { SW_HIDE });
        }
        // Keep pages above the tab control.
        for p in pages {
            SetWindowPos(
                p,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

pub(super) unsafe fn tab_changed(hwnd: HWND) {
    let Some(tab) = with_app(|a| a.panel.tab) else {
        return;
    };
    let sel = unsafe { SendMessageW(tab, TCM_GETCURSEL, 0, 0) }.max(0) as usize;
    with_app(|a| {
        a.panel.cur = sel;
        // Force a full refresh of the newly shown page.
        a.panel.rows = Default::default();
    });
    unsafe {
        super::layout(hwnd);
        refresh(hwnd);
    }
}

fn status_text(s: &Status) -> String {
    match s {
        Status::Paused => "Paused".into(),
        Status::Checking => "Checking files".into(),
        Status::Metadata => "Fetching metadata".into(),
        Status::Downloading => "Downloading".into(),
        Status::Seeding => "Seeding".into(),
        Status::Error(e) => format!("Error: {}", e),
    }
}

fn prio_text(p: u8) -> &'static str {
    match p {
        PRIO_SKIP => "Don't download",
        PRIO_HIGH => "High",
        _ => "Normal",
    }
}

fn tracker_rows(d: &Details) -> Vec<Vec<String>> {
    let now = std::time::Instant::now();
    d.trackers
        .iter()
        .map(|t| {
            let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
            vec![
                t.url.clone(),
                t.info.status.clone(),
                if t.info.status == "Working" {
                    t.info.peers.to_string()
                } else {
                    String::new()
                },
                num(t.info.seeders),
                num(t.info.leechers),
                t.info
                    .next
                    .map(|n| {
                        let s = n.saturating_duration_since(now).as_secs();
                        if s == 0 { "now".into() } else { fmt_eta(s) }
                    })
                    .unwrap_or_default(),
            ]
        })
        .collect()
}

fn peer_rows(d: &Details) -> Vec<Vec<String>> {
    d.peers
        .iter()
        .map(|p| {
            let opt = |n: u64| if n == 0 { String::new() } else { fmt_bytes(n) };
            vec![
                p.addr.to_string(),
                p.client.clone(),
                p.flags.clone(),
                format!("{:.0}%", (p.progress * 100.0).floor()),
                fmt_rate(p.down_rate),
                fmt_rate(p.up_rate),
                opt(p.downloaded),
                opt(p.uploaded),
            ]
        })
        .collect()
}

fn file_rows(d: &Details) -> (Vec<Vec<String>>, Vec<FileRow>) {
    let mut rows = Vec::with_capacity(d.files.len());
    let mut meta = Vec::with_capacity(d.files.len());
    for f in &d.files {
        let progress = if f.size == 0 {
            1.0
        } else {
            f.done as f64 / f.size as f64
        };
        rows.push(vec![
            f.path.clone(),
            fmt_bytes(f.size),
            format!("{:.1}%", (progress * 1000.0).floor() / 10.0),
            prio_text(f.prio).to_string(),
        ]);
        meta.push(FileRow {
            index: f.index,
            prio: f.prio,
            progress,
            rel: f.rel.clone(),
        });
    }
    (rows, meta)
}

/// Update the visible page from the first selected torrent.
pub(super) unsafe fn refresh(_hwnd: HWND) {
    let Some((session, cur)) = with_app(|a| (a.session.clone(), a.panel.cur)) else {
        return;
    };
    let id = unsafe { super::selected_ids() }.first().copied();
    let det = id.and_then(|id| session.details(id));
    let mut new_rows = Vec::new();
    let mut files = None;
    if let Some(d) = &det {
        match cur {
            1 => new_rows = tracker_rows(d),
            2 => new_rows = peer_rows(d),
            3 => {
                let (r, f) = file_rows(d);
                new_rows = r;
                files = Some(f);
            }
            _ => {}
        }
    }
    let urls: Vec<String> = det
        .as_ref()
        .map(|d| d.trackers.iter().map(|t| t.url.clone()).collect())
        .unwrap_or_default();
    let Some((general, list, count_changed, changed, n)) = with_app(|a| {
        let p = &mut a.panel;
        p.snap = det;
        p.tracker_urls = urls;
        if let Some(f) = files {
            p.files = f;
        } else if cur == 3 {
            p.files.clear();
        }
        if cur == 0 {
            return (p.general, None, false, Vec::new(), 0);
        }
        let old = &mut p.rows[cur - 1];
        let count_changed = old.len() != new_rows.len();
        let changed: Vec<usize> = new_rows
            .iter()
            .enumerate()
            .filter(|(i, r)| old.get(*i) != Some(r))
            .map(|(i, _)| i)
            .collect();
        let n = new_rows.len();
        *old = new_rows;
        (p.general, Some(p.lists[cur - 1]), count_changed, changed, n)
    }) else {
        return;
    };
    unsafe {
        match list {
            None => {
                InvalidateRect(general, null(), 0);
            }
            Some(list) => {
                if count_changed {
                    SendMessageW(
                        list,
                        LVM_SETITEMCOUNT,
                        n,
                        (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM,
                    );
                    InvalidateRect(list, null(), 0);
                } else {
                    for i in changed {
                        SendMessageW(list, LVM_REDRAWITEMS, i, i as LPARAM);
                    }
                }
            }
        }
    }
}

/// Text (and check state) for an owner-data details list.
pub(super) unsafe fn dispinfo(which: usize, di: &mut NMLVDISPINFOW) {
    let item = di.item.iItem as usize;
    let sub = di.item.iSubItem as usize;
    unsafe {
        if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() && di.item.cchTextMax > 0 {
            let text = with_app(|a| {
                a.panel.rows[which]
                    .get(item)
                    .and_then(|r| r.get(sub))
                    .cloned()
            })
            .flatten()
            .unwrap_or_default();
            let max = di.item.cchTextMax as usize - 1;
            let mut n = 0;
            for c in text.encode_utf16().take(max) {
                *di.item.pszText.add(n) = c;
                n += 1;
            }
            *di.item.pszText.add(n) = 0;
        }
        if which == 2 && di.item.mask & LVIF_STATE != 0 {
            let on = with_app(|a| a.panel.files.get(item).map(|f| f.prio != PRIO_SKIP))
                .flatten()
                .unwrap_or(false);
            di.item.state = if on { 2 << 12 } else { 1 << 12 };
            di.item.stateMask = LVIS_STATEIMAGEMASK;
        }
    }
}

/// Progress bar in the Files list.
pub(super) unsafe fn draw_file_progress(cd: &NMLVCUSTOMDRAW, list: HWND) {
    let item = cd.nmcd.dwItemSpec;
    let Some((progress, skip, text)) = with_app(|a| {
        a.panel.files.get(item).map(|f| {
            (
                f.progress,
                f.prio == PRIO_SKIP,
                a.panel.rows[2]
                    .get(item)
                    .and_then(|r| r.get(FILE_PROGRESS_COL as usize))
                    .cloned()
                    .unwrap_or_default(),
            )
        })
    })
    .flatten() else {
        return;
    };
    let color = if skip {
        rgb(158, 158, 158)
    } else if progress >= 1.0 {
        rgb(67, 160, 71)
    } else {
        rgb(33, 150, 243)
    };
    unsafe {
        draw_bar(
            cd.nmcd.hdc,
            list,
            item,
            FILE_PROGRESS_COL,
            progress,
            color,
            &text,
        )
    };
}

/// Files of the selected torrent that are selected in the Files list.
unsafe fn selected_files() -> (Option<u64>, Vec<usize>, Vec<PathBuf>) {
    let Some(list) = with_app(|a| a.panel.lists[2]) else {
        return (None, Vec::new(), Vec::new());
    };
    let rows = unsafe { selected_rows(list) };
    with_app(|a| {
        let id = a.panel.snap.as_ref().map(|d| d.id);
        let mut idx = Vec::new();
        let mut paths = Vec::new();
        for r in rows {
            if let Some(f) = a.panel.files.get(r) {
                idx.push(f.index);
                paths.push(f.rel.clone());
            }
        }
        (id, idx, paths)
    })
    .unwrap_or_default()
}

fn save_dir() -> Option<PathBuf> {
    with_app(|a| a.panel.snap.as_ref().map(|d| d.save_dir.clone())).flatten()
}

unsafe fn shell_open(file: &str, params: Option<&str>) {
    let open = w("open");
    let f = w(file);
    let p = params.map(w);
    unsafe {
        ShellExecuteW(
            null_mut(),
            open.as_ptr(),
            f.as_ptr(),
            p.as_ref().map_or(null(), |p| p.as_ptr()),
            null(),
            SW_SHOWNORMAL,
        )
    };
}

/// Handle a details-panel menu command. Returns false if `id` is not one of ours.
pub(super) unsafe fn command(hwnd: HWND, id: usize) -> bool {
    let Some(session) = with_app(|a| a.session.clone()) else {
        return false;
    };
    unsafe {
        match id {
            ID_FILE_NORMAL | ID_FILE_HIGH | ID_FILE_SKIP => {
                let (tid, files, _) = selected_files();
                if let Some(tid) = tid {
                    let prio = match id {
                        ID_FILE_HIGH => PRIO_HIGH,
                        ID_FILE_SKIP => PRIO_SKIP,
                        _ => PRIO_NORMAL,
                    };
                    session.set_file_prio(tid, &files, prio);
                }
            }
            ID_FILE_OPEN | ID_FILE_FOLDER => {
                let (_, _, paths) = selected_files();
                let (Some(dir), Some(rel)) = (save_dir(), paths.first()) else {
                    return true;
                };
                let full = dir.join(rel);
                if id == ID_FILE_OPEN && full.exists() {
                    shell_open(&full.to_string_lossy(), None);
                } else if full.exists() {
                    shell_open(
                        "explorer.exe",
                        Some(&format!("/select,\"{}\"", full.display())),
                    );
                } else if let Some(parent) = full.parent() {
                    shell_open("explorer.exe", Some(&format!("\"{}\"", parent.display())));
                }
            }
            ID_TRACKER_ADD => {
                let Some(tid) = with_app(|a| a.panel.snap.as_ref().map(|d| d.id)).flatten() else {
                    return true;
                };
                if let Some(text) = dialogs::text_input(
                    hwnd,
                    "Add trackers",
                    "Tracker URLs, one per line (http://, https:// or udp://):",
                    "",
                    "Add",
                ) {
                    let urls: Vec<String> = text
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| {
                            let l = l.to_ascii_lowercase();
                            l.starts_with("http://")
                                || l.starts_with("https://")
                                || l.starts_with("udp://")
                        })
                        .collect();
                    if urls.is_empty() && !text.trim().is_empty() {
                        message(
                            hwnd,
                            "No valid tracker URLs were entered.",
                            MB_OK | MB_ICONWARNING,
                        );
                    }
                    session.add_trackers(tid, &urls);
                }
            }
            ID_TRACKER_REMOVE | ID_TRACKER_COPY => {
                let Some(list) = with_app(|a| a.panel.lists[0]) else {
                    return true;
                };
                let rows = selected_rows(list);
                let Some((tid, urls)) = with_app(|a| {
                    let urls: Vec<String> = rows
                        .iter()
                        .filter_map(|&r| a.panel.tracker_urls.get(r).cloned())
                        .collect();
                    a.panel.snap.as_ref().map(|d| (d.id, urls))
                })
                .flatten() else {
                    return true;
                };
                if id == ID_TRACKER_COPY {
                    if !urls.is_empty() {
                        set_clipboard(hwnd, &urls.join("\r\n"));
                    }
                } else {
                    session.remove_trackers(tid, &urls);
                }
            }
            ID_TRACKER_REANNOUNCE => {
                if let Some(tid) = with_app(|a| a.panel.snap.as_ref().map(|d| d.id)).flatten() {
                    session.reannounce(tid);
                }
            }
            ID_PEER_COPY => {
                let Some(list) = with_app(|a| a.panel.lists[1]) else {
                    return true;
                };
                let rows = selected_rows(list);
                let addrs: Vec<String> = with_app(|a| {
                    rows.iter()
                        .filter_map(|&r| a.panel.rows[1].get(r).and_then(|c| c.first()).cloned())
                        .collect()
                })
                .unwrap_or_default();
                if !addrs.is_empty() {
                    set_clipboard(hwnd, &addrs.join("\r\n"));
                }
            }
            _ => return false,
        }
        refresh(hwnd);
        true
    }
}

pub(super) unsafe fn context_menu(hwnd: HWND, which: usize) {
    let has_torrent = with_app(|a| a.panel.snap.is_some()).unwrap_or(false);
    if !has_torrent {
        return;
    }
    let items: &[(usize, &str)] = match which {
        0 => &[
            (ID_TRACKER_ADD, "Add trackers\u{2026}"),
            (ID_TRACKER_REMOVE, "Remove tracker"),
            (ID_TRACKER_COPY, "Copy URL"),
            (0, ""),
            (ID_TRACKER_REANNOUNCE, "Force reannounce"),
        ],
        1 => &[(ID_PEER_COPY, "Copy address")],
        _ => &[
            (ID_FILE_NORMAL, "Download (normal priority)"),
            (ID_FILE_HIGH, "Download first (high priority)"),
            (ID_FILE_SKIP, "Don't download"),
            (0, ""),
            (ID_FILE_OPEN, "Open"),
            (ID_FILE_FOLDER, "Open containing folder"),
        ],
    };
    unsafe { super::popup_menu(hwnd, items) };
}

/// Toggle download of a file when its check box is clicked (or Space is pressed).
pub(super) unsafe fn toggle_files(rows: &[usize]) {
    let Some((session, tid, sel)) = with_app(|a| {
        let sel: Vec<(usize, u8)> = rows
            .iter()
            .filter_map(|&r| a.panel.files.get(r).map(|f| (f.index, f.prio)))
            .collect();
        a.panel
            .snap
            .as_ref()
            .map(|d| (a.session.clone(), d.id, sel))
    })
    .flatten() else {
        return;
    };
    if sel.is_empty() {
        return;
    }
    // Like Explorer: if any selected file is skipped, select them all; else skip them all.
    let any_skipped = sel.iter().any(|&(_, p)| p == PRIO_SKIP);
    let files: Vec<usize> = sel
        .iter()
        .filter(|&&(_, p)| if any_skipped { p == PRIO_SKIP } else { true })
        .map(|&(i, _)| i)
        .collect();
    session.set_file_prio(
        tid,
        &files,
        if any_skipped { PRIO_NORMAL } else { PRIO_SKIP },
    );
}

pub(super) unsafe fn on_files_click(list: HWND, lparam: LPARAM) {
    let act = unsafe { &*(lparam as *const NMITEMACTIVATE) };
    let mut ht: LVHITTESTINFO = unsafe { std::mem::zeroed() };
    ht.pt = act.ptAction;
    let r = unsafe { SendMessageW(list, LVM_HITTEST, 0, &mut ht as *mut _ as LPARAM) };
    if r >= 0 && ht.flags & LVHT_ONITEMSTATEICON != 0 {
        unsafe {
            toggle_files(&[r as usize]);
            refresh(null_mut());
        }
    }
}

pub(super) unsafe fn on_files_key(list: HWND, key: u16) {
    if key == VK_SPACE {
        unsafe {
            let rows = selected_rows(list);
            toggle_files(&rows);
            refresh(null_mut());
        }
    }
}

// ---- General page ----

fn fmt_time(unix: u64) -> String {
    if unix == 0 {
        return "\u{2014}".into();
    }
    unsafe {
        let t = (unix + 11_644_473_600) * 10_000_000;
        let ft = FILETIME {
            dwLowDateTime: t as u32,
            dwHighDateTime: (t >> 32) as u32,
        };
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) == 0
        {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
        )
    }
}

fn with_limit(rate: u64, limit: u64) -> String {
    let r = if rate == 0 {
        "0 B/s".to_string()
    } else {
        fmt_rate(rate)
    };
    if limit > 0 {
        format!("{}  (limit {})", r, fmt_rate(limit))
    } else {
        r
    }
}

type Lines = Vec<(&'static str, String)>;

/// (left column, right column, full-width rows) of the General page.
fn general_lines(d: &Details) -> (Lines, Lines, Lines) {
    let ratio = if d.have_bytes > 0 {
        format!("{:.2}", d.uploaded as f64 / d.have_bytes as f64)
    } else {
        "\u{2014}".into()
    };
    let left = vec![
        ("Status:", status_text(&d.status)),
        ("Download speed:", with_limit(d.down_rate, d.down_limit)),
        ("Upload speed:", with_limit(d.up_rate, d.up_limit)),
        (
            "Downloaded:",
            format!("{} (this session)", fmt_bytes(d.downloaded)),
        ),
        ("Uploaded:", fmt_bytes(d.uploaded)),
        ("Share ratio:", ratio),
        (
            "Peers:",
            format!("{} connected, {} known", d.peers.len(), d.known_peers),
        ),
    ];
    let right = if d.has_meta {
        vec![
            ("Total size:", fmt_bytes(d.total_size)),
            ("Selected:", fmt_bytes(d.wanted_size)),
            (
                "Pieces:",
                format!(
                    "{} \u{d7} {} (have {})",
                    d.num_pieces,
                    fmt_bytes(d.piece_length),
                    d.have_pieces
                ),
            ),
            ("Files:", d.files.len().to_string()),
            ("Added on:", fmt_time(d.added_on)),
            ("Completed on:", fmt_time(d.completed_on)),
            ("Trackers:", d.trackers.len().to_string()),
        ]
    } else {
        vec![
            ("Total size:", "unknown (waiting for metadata)".into()),
            ("Added on:", fmt_time(d.added_on)),
            ("Trackers:", d.trackers.len().to_string()),
        ]
    };
    let mut full = vec![
        ("Save path:", d.save_dir.join(&d.name).display().to_string()),
        ("Info hash v1:", hex(&d.info_hash)),
    ];
    if let Some(v2) = d.info_hash_v2 {
        full.push(("Info hash v2:", format!("{} (truncated)", hex(&v2))));
    }
    (left, right, full)
}

/// Paint the pieces bar: one column per pixel, coloured by how much of it we have.
unsafe fn paint_pieces(hdc: HDC, rc: RECT, pieces: &[u8]) {
    unsafe {
        let frame = CreateSolidBrush(rgb(190, 190, 190));
        FrameRect(hdc, &rc, frame);
        DeleteObject(frame as HGDIOBJ);
        let inner = RECT {
            left: rc.left + 1,
            top: rc.top + 1,
            right: rc.right - 1,
            bottom: rc.bottom - 1,
        };
        let width = (inner.right - inner.left).max(0) as usize;
        if width == 0 {
            return;
        }
        let n = pieces.len();
        let colors: Vec<COLORREF> = (0..width)
            .map(|x| {
                if n == 0 {
                    return rgb(240, 240, 240);
                }
                let a = x * n / width;
                let b = ((x + 1) * n / width).max(a + 1).min(n);
                let range = &pieces[a..b];
                let have = range.iter().filter(|&&p| p == 2).count() as f64 / range.len() as f64;
                if range.contains(&1) {
                    rgb(76, 175, 80)
                } else if have > 0.0 {
                    let mix = |lo: u8, hi: u8| (lo as f64 + (hi as f64 - lo as f64) * have) as u8;
                    rgb(mix(187, 25), mix(222, 118), mix(251, 210))
                } else if range.iter().all(|&p| p == 3) {
                    rgb(205, 205, 205)
                } else {
                    rgb(240, 240, 240)
                }
            })
            .collect();
        let mut x = 0;
        while x < width {
            let mut end = x + 1;
            while end < width && colors[end] == colors[x] {
                end += 1;
            }
            let col = RECT {
                left: inner.left + x as i32,
                top: inner.top,
                right: inner.left + end as i32,
                bottom: inner.bottom,
            };
            let br = CreateSolidBrush(colors[x]);
            FillRect(hdc, &col, br);
            DeleteObject(br as HGDIOBJ);
            x = end;
        }
    }
}

unsafe fn draw_text(hdc: HDC, s: &str, rc: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut t: Vec<u16> = s.encode_utf16().collect();
    unsafe { DrawTextW(hdc, t.as_mut_ptr(), t.len() as i32, rc, flags) };
}

pub(super) unsafe extern "system" fn general_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                // Double-buffer to avoid flicker on every refresh.
                let mem = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, rc.right.max(1), rc.bottom.max(1));
                let old_bmp = SelectObject(mem, bmp as HGDIOBJ);
                FillRect(mem, &rc, GetSysColorBrush(COLOR_WINDOW));
                let (font, dpi) = with_app(|a| (a.font, a.dpi)).unwrap_or((0, 96));
                let old_font = SelectObject(mem, font as HGDIOBJ);
                SetBkMode(mem, TRANSPARENT as _);
                let s = |v: i32| scale(v, dpi);
                let pad = s(10);
                let line = s(20);
                let label_w = s(120);
                let content = with_app(|a| {
                    a.panel
                        .snap
                        .as_ref()
                        .map(|d| (d.pieces.clone(), general_lines(d)))
                })
                .flatten();
                match content {
                    None => {
                        SetTextColor(mem, rgb(120, 120, 120));
                        let mut r = RECT {
                            left: pad,
                            top: pad,
                            right: rc.right - pad,
                            bottom: rc.bottom - pad,
                        };
                        draw_text(
                            mem,
                            "Select a torrent to see its details.",
                            &mut r,
                            DT_LEFT | DT_SINGLELINE,
                        );
                    }
                    Some((pieces, (left, right, full))) => {
                        let mut y = pad;
                        SetTextColor(mem, rgb(100, 100, 100));
                        let mut r = RECT {
                            left: pad,
                            top: y,
                            right: pad + label_w,
                            bottom: y + s(18),
                        };
                        draw_text(mem, "Pieces:", &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                        paint_pieces(
                            mem,
                            RECT {
                                left: pad + label_w,
                                top: y,
                                right: rc.right - pad,
                                bottom: y + s(18),
                            },
                            &pieces,
                        );
                        y += s(30);
                        let col_w = ((rc.right - 2 * pad) / 2).max(s(200));
                        let draw_pairs = |pairs: &[(&str, String)], x: i32, y0: i32, width: i32| {
                            let mut y = y0;
                            for (k, v) in pairs {
                                SetTextColor(mem, rgb(100, 100, 100));
                                let mut r = RECT {
                                    left: x,
                                    top: y,
                                    right: x + label_w,
                                    bottom: y + line,
                                };
                                draw_text(mem, k, &mut r, DT_LEFT | DT_SINGLELINE);
                                SetTextColor(mem, GetSysColor(COLOR_WINDOWTEXT));
                                let mut r = RECT {
                                    left: x + label_w,
                                    top: y,
                                    right: x + width - s(8),
                                    bottom: y + line,
                                };
                                draw_text(
                                    mem,
                                    v,
                                    &mut r,
                                    DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                                );
                                y += line;
                            }
                            y
                        };
                        let y1 = draw_pairs(&left, pad, y, col_w);
                        let y2 = draw_pairs(&right, pad + col_w, y, col_w);
                        let y = y1.max(y2) + s(8);
                        draw_pairs(&full, pad, y, rc.right - 2 * pad);
                    }
                }
                BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old_font);
                SelectObject(mem, old_bmp);
                DeleteObject(bmp as HGDIOBJ);
                DeleteDC(mem);
                EndPaint(hwnd, &ps);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
