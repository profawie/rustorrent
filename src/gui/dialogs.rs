//! Modal dialogs built from plain Win32 controls: settings, speed limits, add torrent and
//! text input.

use super::{browse_folder, scale, w, with_app};
use crate::metainfo::Metainfo;
use crate::session::{AddOptions, Settings};
use crate::util::fmt_bytes;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub(super) const DLG_CLASS: &str = "RustorrentDialog";
const ID_LIST_CHANGED: usize = 0xF001;

thread_local! {
    static QUEUE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn push(id: usize) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if q.last() != Some(&id) {
            q.push(id);
        }
    });
}

pub(super) unsafe extern "system" fn dlg_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                // Button clicks (and Enter/Escape from IsDialogMessage) only.
                if (wparam >> 16) & 0xffff == BN_CLICKED as usize {
                    push(wparam & 0xffff);
                }
                0
            }
            WM_NOTIFY => {
                let hdr = &*(lparam as *const NMHDR);
                if hdr.code == LVN_ITEMCHANGED {
                    push(ID_LIST_CHANGED);
                }
                0
            }
            WM_CLOSE => {
                push(IDCANCEL as usize);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

pub(super) struct Dlg {
    pub hwnd: HWND,
    parent: HWND,
    font: WPARAM,
    dpi: i32,
    hinst: HINSTANCE,
    /// Multi-line edit in which Enter confirms the dialog.
    enter_edit: Cell<HWND>,
}

impl Dlg {
    /// Create a hidden dialog window with a client area of `cw` x `ch` (96-dpi units).
    pub unsafe fn new(parent: HWND, title: &str, cw: i32, ch: i32) -> Option<Dlg> {
        let dpi = with_app(|a| a.dpi).unwrap_or(96);
        let font = with_app(|a| a.font).unwrap_or(0);
        unsafe {
            let hinst = GetModuleHandleW(null());
            let class = w(DLG_CLASS);
            let t = w(title);
            let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
            let ex = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: scale(cw, dpi),
                bottom: scale(ch, dpi),
            };
            AdjustWindowRectEx(&mut rc, style, 0, ex);
            let (width, height) = (rc.right - rc.left, rc.bottom - rc.top);
            let mut prc: RECT = std::mem::zeroed();
            GetWindowRect(parent, &mut prc);
            let x = (prc.left + ((prc.right - prc.left) - width) / 2).max(0);
            let y = (prc.top + ((prc.bottom - prc.top) - height) / 2).max(0);
            let hwnd = CreateWindowExW(
                ex,
                class.as_ptr(),
                t.as_ptr(),
                style,
                x,
                y,
                width,
                height,
                parent,
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            Some(Dlg {
                hwnd,
                parent,
                font,
                dpi,
                hinst,
                enter_edit: Cell::new(null_mut()),
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub unsafe fn child(
        &self,
        class: &str,
        text: &str,
        style: u32,
        ex: u32,
        rect: (i32, i32, i32, i32),
        id: usize,
    ) -> HWND {
        let s = |v| scale(v, self.dpi);
        let c = w(class);
        let t = w(text);
        unsafe {
            let h = CreateWindowExW(
                ex,
                c.as_ptr(),
                t.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                s(rect.0),
                s(rect.1),
                s(rect.2),
                s(rect.3),
                self.hwnd,
                id as HMENU,
                self.hinst,
                null(),
            );
            SendMessageW(h, WM_SETFONT, self.font, 1);
            h
        }
    }

    pub unsafe fn label(&self, text: &str, rect: (i32, i32, i32, i32)) -> HWND {
        unsafe { self.child("STATIC", text, 0, 0, rect, 0) }
    }

    pub unsafe fn edit(
        &self,
        text: &str,
        rect: (i32, i32, i32, i32),
        id: usize,
        style: u32,
    ) -> HWND {
        unsafe {
            self.child(
                "EDIT",
                text,
                WS_TABSTOP | ES_AUTOHSCROLL as u32 | style,
                WS_EX_CLIENTEDGE,
                rect,
                id,
            )
        }
    }

    pub unsafe fn check(
        &self,
        text: &str,
        on: bool,
        rect: (i32, i32, i32, i32),
        id: usize,
    ) -> HWND {
        unsafe {
            let h = self.child(
                "BUTTON",
                text,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                0,
                rect,
                id,
            );
            SendMessageW(h, BM_SETCHECK, if on { 1 } else { 0 }, 0);
            h
        }
    }

    pub unsafe fn button(
        &self,
        text: &str,
        rect: (i32, i32, i32, i32),
        id: usize,
        default: bool,
    ) -> HWND {
        let style = if default {
            BS_DEFPUSHBUTTON
        } else {
            BS_PUSHBUTTON
        };
        unsafe { self.child("BUTTON", text, WS_TABSTOP | style as u32, 0, rect, id) }
    }

    pub unsafe fn group(&self, text: &str, rect: (i32, i32, i32, i32)) -> HWND {
        unsafe { self.child("BUTTON", text, BS_GROUPBOX as u32, 0, rect, 0) }
    }

    pub unsafe fn combo(
        &self,
        items: &[&str],
        sel: usize,
        rect: (i32, i32, i32, i32),
        id: usize,
    ) -> HWND {
        unsafe {
            let h = self.child(
                "COMBOBOX",
                "",
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                0,
                rect,
                id,
            );
            for it in items {
                let t = w(it);
                SendMessageW(h, CB_ADDSTRING, 0, t.as_ptr() as LPARAM);
            }
            SendMessageW(h, CB_SETCURSEL, sel, 0);
            h
        }
    }

    /// OK and Cancel buttons in the bottom-right corner of a `cw` x `ch` client area.
    pub unsafe fn ok_cancel(&self, ok: &str, cw: i32, ch: i32) {
        unsafe {
            self.button(
                ok,
                (cw - 10 - 2 * 88 - 6, ch - 38, 88, 28),
                IDOK as usize,
                true,
            );
            self.button(
                "Cancel",
                (cw - 10 - 88, ch - 38, 88, 28),
                IDCANCEL as usize,
                false,
            );
        }
    }

    /// Run the dialog modally. `on_cmd` gets every button id; returning `Some(r)` closes the
    /// dialog with result `r`.
    pub unsafe fn run(&self, focus: HWND, mut on_cmd: impl FnMut(usize) -> Option<bool>) -> bool {
        QUEUE.with(|q| q.borrow_mut().clear());
        unsafe {
            EnableWindow(self.parent, 0);
            ShowWindow(self.hwnd, SW_SHOW);
            SetFocus(focus);
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, null_mut(), 0, 0);
                if r <= 0 {
                    PostQuitMessage(0);
                    return false;
                }
                let enter_edit = self.enter_edit.get();
                if msg.message == WM_KEYDOWN
                    && msg.wParam == VK_RETURN as usize
                    && !enter_edit.is_null()
                    && msg.hwnd == enter_edit
                {
                    push(IDOK as usize);
                } else if IsDialogMessageW(self.hwnd, &msg) == 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let ids: Vec<usize> = QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
                for id in ids {
                    if let Some(r) = on_cmd(id) {
                        return r;
                    }
                }
            }
        }
    }
}

impl Drop for Dlg {
    fn drop(&mut self) {
        unsafe {
            EnableWindow(self.parent, 1);
            SetForegroundWindow(self.parent);
            DestroyWindow(self.hwnd);
        }
    }
}

pub(super) unsafe fn text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..len])
    }
}

unsafe fn set_text(h: HWND, s: &str) {
    let t = w(s);
    unsafe { SetWindowTextW(h, t.as_ptr()) };
}

unsafe fn checked(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

/// Parse a whole number from an edit box; empty means 0.
unsafe fn number(h: HWND) -> Option<u64> {
    let t = unsafe { text(h) };
    let t = t.trim();
    if t.is_empty() {
        Some(0)
    } else {
        t.parse().ok()
    }
}

unsafe fn invalid(dlg: &Dlg, field: HWND, what: &str) -> Option<bool> {
    unsafe {
        super::message(dlg.hwnd, what, MB_OK | MB_ICONWARNING);
        SetFocus(field);
        SendMessageW(field, EM_SETSEL, 0, -1);
    }
    None
}

// ---- settings ----

const ID_DIR: usize = 1001;
const ID_BROWSE: usize = 1002;

/// Show the settings dialog; returns the new settings if the user pressed OK.
pub(super) unsafe fn settings(parent: HWND, cur: &Settings, actual_port: u16) -> Option<Settings> {
    let (cw, ch) = (470, 468);
    unsafe {
        let d = Dlg::new(parent, "Settings", cw, ch)?;
        d.group("Downloads", (10, 6, 450, 94));
        d.label("Save new downloads to:", (22, 28, 200, 18));
        let dir = d.edit(
            &cur.download_dir.to_string_lossy(),
            (22, 48, 334, 24),
            ID_DIR,
            0,
        );
        d.button("Browse\u{2026}", (364, 47, 84, 26), ID_BROWSE, false);
        let ask = d.check(
            "Ask which files to download when adding a torrent",
            cur.ask_files,
            (22, 76, 420, 20),
            0,
        );

        d.group("Connection", (10, 108, 450, 132));
        d.label("Listening port:", (22, 132, 150, 18));
        let port = d.edit(
            &cur.port.to_string(),
            (180, 129, 80, 24),
            0,
            ES_NUMBER as u32,
        );
        d.label(
            &format!("0 = automatic (now {})", actual_port),
            (270, 132, 180, 18),
        );
        let upnp = d.check(
            "Forward the port on my router (UPnP / NAT-PMP)",
            cur.upnp,
            (22, 160, 420, 20),
            0,
        );
        let dht = d.check(
            "Enable DHT (find peers without trackers)",
            cur.dht,
            (22, 182, 420, 20),
            0,
        );
        d.label("Max connections:", (22, 211, 150, 18));
        let conns = d.edit(
            &cur.max_conns.to_string(),
            (180, 208, 70, 24),
            0,
            ES_NUMBER as u32,
        );
        d.label("Per torrent:", (270, 211, 90, 18));
        let peers = d.edit(
            &cur.max_peers.to_string(),
            (366, 208, 70, 24),
            0,
            ES_NUMBER as u32,
        );

        d.group("Speed limits (KB/s, 0 = unlimited)", (10, 248, 450, 62));
        d.label("Download:", (22, 274, 80, 18));
        let down = d.edit(
            &(cur.down_limit / 1024).to_string(),
            (104, 271, 90, 24),
            0,
            ES_NUMBER as u32,
        );
        d.label("Upload:", (232, 274, 70, 18));
        let up = d.edit(
            &(cur.up_limit / 1024).to_string(),
            (306, 271, 90, 24),
            0,
            ES_NUMBER as u32,
        );

        d.group("Privacy", (10, 318, 450, 62));
        d.label("Protocol encryption:", (22, 344, 150, 18));
        let enc = d.combo(
            &[
                "Prefer encrypted connections",
                "Require encryption",
                "Disable encryption",
            ],
            cur.encryption as usize,
            (180, 340, 268, 200),
            0,
        );
        d.label(
            "Changes to the port and DHT take effect after restarting rustorrent.",
            (12, 392, 446, 18),
        );
        d.ok_cancel("OK", cw, ch);

        let mut out = None;
        let ok = d.run(dir, |id| match id {
            ID_BROWSE => {
                if let Some(p) = browse_folder(d.hwnd) {
                    set_text(dir, &p.to_string_lossy());
                }
                None
            }
            x if x == IDCANCEL as usize => Some(false),
            x if x == IDOK as usize => {
                let folder = text(dir).trim().to_string();
                if folder.is_empty() {
                    return invalid(&d, dir, "Please choose a download folder.");
                }
                let Some(p) = number(port).filter(|&p| p <= 65535) else {
                    return invalid(&d, port, "The port must be a number from 0 to 65535.");
                };
                let Some(c) = number(conns).filter(|&c| (10..=5000).contains(&c)) else {
                    return invalid(&d, conns, "Max connections must be between 10 and 5000.");
                };
                let Some(pc) = number(peers).filter(|&c| (5..=1000).contains(&c)) else {
                    return invalid(
                        &d,
                        peers,
                        "Connections per torrent must be between 5 and 1000.",
                    );
                };
                let Some(dl) = number(down).filter(|&v| v < 10_000_000) else {
                    return invalid(&d, down, "Please enter the download limit in KB/s.");
                };
                let Some(ul) = number(up).filter(|&v| v < 10_000_000) else {
                    return invalid(&d, up, "Please enter the upload limit in KB/s.");
                };
                let e = SendMessageW(enc, CB_GETCURSEL, 0, 0).clamp(0, 2) as u8;
                out = Some(Settings {
                    port: p as u16,
                    upnp: checked(upnp),
                    dht: checked(dht),
                    encryption: e,
                    down_limit: dl * 1024,
                    up_limit: ul * 1024,
                    max_conns: c as usize,
                    max_peers: pc as usize,
                    download_dir: PathBuf::from(folder),
                    ask_files: checked(ask),
                });
                Some(true)
            }
            _ => None,
        });
        if ok { out } else { None }
    }
}

// ---- per-torrent speed limit ----

/// Ask for download/upload limits (bytes/s). `cur` pre-fills the fields.
pub(super) unsafe fn speed_limit(
    parent: HWND,
    count: usize,
    cur: (u64, u64),
) -> Option<(u64, u64)> {
    let (cw, ch) = (340, 150);
    unsafe {
        let d = Dlg::new(parent, "Torrent speed limit", cw, ch)?;
        let what = if count == 1 {
            "this torrent".to_string()
        } else {
            format!("{} torrents", count)
        };
        d.label(
            &format!("Limits for {} in KB/s (0 = unlimited):", what),
            (12, 12, 316, 18),
        );
        d.label("Download:", (12, 43, 90, 18));
        let down = d.edit(
            &(cur.0 / 1024).to_string(),
            (110, 40, 100, 24),
            0,
            ES_NUMBER as u32,
        );
        d.label("Upload:", (12, 75, 90, 18));
        let up = d.edit(
            &(cur.1 / 1024).to_string(),
            (110, 72, 100, 24),
            0,
            ES_NUMBER as u32,
        );
        d.ok_cancel("OK", cw, ch);
        SendMessageW(down, EM_SETSEL, 0, -1);
        let mut out = None;
        let ok = d.run(down, |id| match id {
            x if x == IDCANCEL as usize => Some(false),
            x if x == IDOK as usize => {
                let Some(dl) = number(down).filter(|&v| v < 10_000_000) else {
                    return invalid(&d, down, "Please enter a number in KB/s.");
                };
                let Some(ul) = number(up).filter(|&v| v < 10_000_000) else {
                    return invalid(&d, up, "Please enter a number in KB/s.");
                };
                out = Some((dl * 1024, ul * 1024));
                Some(true)
            }
            _ => None,
        });
        if ok { out } else { None }
    }
}

// ---- multi-line text input (magnet links, trackers) ----

pub(super) unsafe fn text_input(
    parent: HWND,
    title: &str,
    prompt: &str,
    initial: &str,
    ok: &str,
) -> Option<String> {
    let (cw, ch) = (560, 240);
    unsafe {
        let d = Dlg::new(parent, title, cw, ch)?;
        d.label(prompt, (10, 10, 540, 18));
        let initial = initial.replace("\r\n", "\n").replace('\n', "\r\n");
        let edit = d.child(
            "EDIT",
            &initial,
            WS_TABSTOP | WS_VSCROLL | (ES_MULTILINE | ES_AUTOVSCROLL) as u32,
            WS_EX_CLIENTEDGE,
            (10, 32, 540, 158),
            0,
        );
        d.enter_edit.set(edit);
        d.ok_cancel(ok, cw, ch);
        SendMessageW(edit, EM_SETSEL, 0, -1);
        let mut out = None;
        let r = d.run(edit, |id| match id {
            x if x == IDCANCEL as usize => Some(false),
            x if x == IDOK as usize => {
                out = Some(text(edit));
                Some(true)
            }
            _ => None,
        });
        if r { out } else { None }
    }
}

// ---- add torrent with file selection ----

const ID_SAVE_DIR: usize = 1101;
const ID_SAVE_BROWSE: usize = 1102;
const ID_ALL: usize = 1105;
const ID_NONE: usize = 1106;

const STATE_CHECKED: u32 = 2 << 12;
const STATE_UNCHECKED: u32 = 1 << 12;

unsafe fn set_check(list: HWND, i: usize, on: bool) {
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.stateMask = LVIS_STATEIMAGEMASK;
    item.state = if on { STATE_CHECKED } else { STATE_UNCHECKED };
    unsafe { SendMessageW(list, LVM_SETITEMSTATE, i, &item as *const _ as LPARAM) };
}

unsafe fn is_checked(list: HWND, i: usize) -> bool {
    unsafe {
        SendMessageW(list, LVM_GETITEMSTATE, i, LVIS_STATEIMAGEMASK as LPARAM) as u32
            & LVIS_STATEIMAGEMASK
            == STATE_CHECKED
    }
}

/// Let the user pick files and a folder for a new torrent. Returns None if cancelled.
pub(super) unsafe fn add_torrent(
    parent: HWND,
    meta: &Metainfo,
    default_dir: &std::path::Path,
) -> Option<AddOptions> {
    let (cw, ch) = (640, 470);
    let files: Vec<(usize, String, u64)> = meta
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.pad)
        .map(|(i, f)| {
            let rel = f.path.strip_prefix(&meta.name).unwrap_or(&f.path);
            let name = if rel.as_os_str().is_empty() {
                f.path.to_string_lossy().into_owned()
            } else {
                rel.to_string_lossy().into_owned()
            };
            (i, name, f.length)
        })
        .collect();
    unsafe {
        let title = format!("Add torrent \u{2013} {}", meta.name);
        let d = Dlg::new(parent, &title, cw, ch)?;
        d.label("Save to:", (10, 14, 60, 18));
        let dir = d.edit(
            &default_dir.to_string_lossy(),
            (72, 10, 462, 24),
            ID_SAVE_DIR,
            0,
        );
        d.button("Browse\u{2026}", (542, 9, 88, 26), ID_SAVE_BROWSE, false);
        let list = d.child(
            "SysListView32",
            "",
            WS_TABSTOP | LVS_REPORT | LVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            (10, 44, 620, 300),
            0,
        );
        let ex = LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            ex as WPARAM,
            ex as LPARAM,
        );
        for (i, (name, width, right)) in [("Name", 470, false), ("Size", 110, true)]
            .iter()
            .enumerate()
        {
            let mut t = w(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            col.cx = scale(*width, d.dpi);
            col.pszText = t.as_mut_ptr();
            col.iSubItem = i as i32;
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
        }
        SendMessageW(list, LVM_SETITEMCOUNT, files.len(), 0);
        for (row, (_, name, size)) in files.iter().enumerate() {
            let mut t = w(name);
            let mut item: LVITEMW = std::mem::zeroed();
            item.mask = LVIF_TEXT;
            item.iItem = row as i32;
            item.pszText = t.as_mut_ptr();
            SendMessageW(list, LVM_INSERTITEMW, 0, &item as *const _ as LPARAM);
            let mut t = w(&fmt_bytes(*size));
            item.iSubItem = 1;
            item.pszText = t.as_mut_ptr();
            SendMessageW(list, LVM_SETITEMTEXTW, row, &item as *const _ as LPARAM);
            set_check(list, row, true);
        }
        let summary = d.label("", (10, 354, 400, 18));
        d.button("Select all", (432, 350, 96, 26), ID_ALL, false);
        d.button("Select none", (534, 350, 96, 26), ID_NONE, false);
        let start = d.check("Start downloading", true, (10, 388, 300, 20), 0);
        d.ok_cancel("Add", cw, ch);

        let update = || {
            let mut sel = 0u64;
            let mut n = 0;
            for (row, f) in files.iter().enumerate() {
                if is_checked(list, row) {
                    sel += f.2;
                    n += 1;
                }
            }
            set_text(
                summary,
                &format!(
                    "Selected {} of {} files: {} of {}",
                    n,
                    files.len(),
                    fmt_bytes(sel),
                    fmt_bytes(meta.total_length)
                ),
            );
        };
        update();
        let mut out = None;
        let ok = d.run(list, |id| match id {
            ID_SAVE_BROWSE => {
                if let Some(p) = browse_folder(d.hwnd) {
                    set_text(dir, &p.to_string_lossy());
                }
                None
            }
            ID_ALL | ID_NONE => {
                for row in 0..files.len() {
                    set_check(list, row, id == ID_ALL);
                }
                update();
                None
            }
            ID_LIST_CHANGED => {
                update();
                None
            }
            x if x == IDCANCEL as usize => Some(false),
            x if x == IDOK as usize => {
                let folder = text(dir).trim().to_string();
                if folder.is_empty() {
                    return invalid(&d, dir, "Please choose where to save the download.");
                }
                let mut prio = vec![1u8; meta.files.len()];
                let mut any = false;
                for (row, f) in files.iter().enumerate() {
                    let on = is_checked(list, row);
                    prio[f.0] = on as u8;
                    any |= on;
                }
                if !any {
                    super::message(
                        d.hwnd,
                        "Select at least one file to download.",
                        MB_OK | MB_ICONWARNING,
                    );
                    return None;
                }
                out = Some(AddOptions {
                    save_dir: Some(PathBuf::from(folder)),
                    file_prio: Some(prio),
                    paused: !checked(start),
                });
                Some(true)
            }
            _ => None,
        });
        if ok { out } else { None }
    }
}
