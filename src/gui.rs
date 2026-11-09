//! Native Win32 user interface (no GUI framework: keeps the executable small).

use crate::session::{Config, Session};
use crate::torrent::{Stats, Status};
use crate::util::{fmt_bytes, fmt_eta, fmt_rate};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const MAIN_CLASS: &str = "RustorrentMainWindow";
const INPUT_CLASS: &str = "RustorrentInputWindow";
const CF_UNICODETEXT: u32 = 13;

const IDC_LIST: usize = 100;
const IDC_STATUS: usize = 101;
const IDC_INPUT_EDIT: usize = 102;

const ID_ADD: usize = 201;
const ID_MAGNET: usize = 202;
const ID_START: usize = 203;
const ID_PAUSE: usize = 204;
const ID_REMOVE: usize = 205;
const ID_OPEN: usize = 206;
const ID_SETDIR: usize = 207;
const ID_REMOVE_DELETE: usize = 208;
const ID_COPY_MAGNET: usize = 209;
const ID_RECHECK: usize = 210;
const TIMER_ID: usize = 1;

const BUTTONS: [(&str, usize, i32); 7] = [
    ("Add torrent\u{2026}", ID_ADD, 104),
    ("Add magnet\u{2026}", ID_MAGNET, 104),
    ("Start", ID_START, 64),
    ("Pause", ID_PAUSE, 64),
    ("Remove", ID_REMOVE, 72),
    ("Open folder", ID_OPEN, 92),
    ("Download folder\u{2026}", ID_SETDIR, 124),
];

const NCOLS: usize = 9;
const COLUMNS: [(&str, i32, bool); NCOLS] = [
    ("Name", 300, false),
    ("Size", 75, true),
    ("Progress", 110, false),
    ("Status", 110, false),
    ("Down", 85, true),
    ("Up", 85, true),
    ("Peers", 60, true),
    ("ETA", 70, true),
    ("Uploaded", 80, true),
];
const PROGRESS_COL: i32 = 2;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(PartialEq)]
struct Row {
    id: u64,
    cells: [String; NCOLS],
    progress: f64,
    color: COLORREF,
}

struct App {
    session: Arc<Session>,
    list: HWND,
    status: HWND,
    buttons: Vec<HWND>,
    rows: Vec<Row>,
    dpi: i32,
    ticks: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static INPUT_DONE: Cell<bool> = const { Cell::new(false) };
    static INPUT_TEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Run `f` with the app state. Never panics on re-entrancy: returns None instead.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn scale(v: i32, dpi: i32) -> i32 {
    v * dpi / 96
}

fn make_row(s: &Stats) -> Row {
    let (status, color) = match &s.status {
        Status::Paused => ("Paused".to_string(), rgb(158, 158, 158)),
        Status::Checking => ("Checking".to_string(), rgb(255, 152, 0)),
        Status::Metadata => ("Fetching metadata".to_string(), rgb(255, 152, 0)),
        Status::Downloading => ("Downloading".to_string(), rgb(33, 150, 243)),
        Status::Seeding => ("Seeding".to_string(), rgb(67, 160, 71)),
        Status::Error(e) => (format!("Error: {}", e), rgb(229, 57, 53)),
    };
    let active = !matches!(s.status, Status::Paused | Status::Error(_));
    Row {
        id: s.id,
        cells: [
            s.name.clone(),
            if s.size > 0 {
                fmt_bytes(s.size)
            } else {
                String::new()
            },
            format!("{:.1}%", (s.progress * 1000.0).floor() / 10.0),
            status,
            fmt_rate(s.down_rate),
            fmt_rate(s.up_rate),
            if active {
                format!("{} ({})", s.peers, s.known)
            } else {
                String::new()
            },
            fmt_eta(s.eta),
            if s.uploaded > 0 {
                fmt_bytes(s.uploaded)
            } else {
                String::new()
            },
        ],
        progress: s.progress,
        color,
    }
}

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    unsafe {
        let class = w(MAIN_CLASS);
        // Single instance: hand arguments to the running window and leave.
        let existing = FindWindowW(class.as_ptr(), null());
        if !existing.is_null() {
            for a in &args {
                send_copydata(existing, a);
            }
            if IsIconic(existing) != 0 {
                ShowWindow(existing, SW_RESTORE);
            }
            SetForegroundWindow(existing);
            return;
        }
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let hinst = GetModuleHandleW(null());
        #[allow(clippy::manual_dangling_ptr)] // MAKEINTRESOURCE(1)
        let icon = LoadIconW(hinst, 1 as *const u16);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(main_proc);
        wc.hInstance = hinst;
        wc.hIcon = icon;
        wc.hIconSm = icon;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hbrBackground = (COLOR_BTNFACE + 1) as usize as HBRUSH;
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        let input_class = w(INPUT_CLASS);
        wc.lpfnWndProc = Some(input_proc);
        wc.lpszClassName = input_class.as_ptr();
        RegisterClassExW(&wc);

        let session = Session::new(Config {
            data_dir: crate::data_dir(),
            port: 0,
            dht: true,
            persist: true,
            download_dir: crate::default_download_dir(),
        });

        let hdc = GetDC(null_mut());
        let dpi = GetDeviceCaps(hdc, LOGPIXELSY as _).max(96);
        ReleaseDC(null_mut(), hdc);

        let title = w("rustorrent");
        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(1020, dpi),
            scale(540, dpi),
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        create_children(hwnd, hinst, session, dpi);
        layout(hwnd);
        DragAcceptFiles(hwnd, 1);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetTimer(hwnd, TIMER_ID, 1000, None);
        for a in &args {
            add_item(hwnd, a);
        }
        tick(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Ctrl+V anywhere in the main window adds magnet links from the clipboard.
            if msg.message == WM_KEYDOWN
                && msg.wParam == b'V' as usize
                && GetKeyState(VK_CONTROL as i32) < 0
            {
                if let Some(text) = clipboard_text(hwnd) {
                    if text
                        .trim_start()
                        .to_ascii_lowercase()
                        .starts_with("magnet:")
                    {
                        add_lines(hwnd, &text);
                        continue;
                    }
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn send_copydata(hwnd: HWND, s: &str) {
    let data: Vec<u16> = s.encode_utf16().collect();
    let cds = COPYDATASTRUCT {
        dwData: 1,
        cbData: (data.len() * 2) as u32,
        lpData: data.as_ptr() as *mut c_void,
    };
    unsafe { SendMessageW(hwnd, WM_COPYDATA, 0, &cds as *const _ as LPARAM) };
}

unsafe fn create_children(hwnd: HWND, hinst: HINSTANCE, session: Arc<Session>, dpi: i32) {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            &mut ncm as *mut _ as *mut c_void,
            0,
        );
        let font = CreateFontIndirectW(&ncm.lfMessageFont);

        let mut buttons = Vec::new();
        let button_class = w("BUTTON");
        for (text, id, _) in BUTTONS {
            let t = w(text);
            let b = CreateWindowExW(
                0,
                button_class.as_ptr(),
                t.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
                0,
                10,
                10,
                hwnd,
                id as HMENU,
                hinst,
                null(),
            );
            SendMessageW(b, WM_SETFONT, font as WPARAM, 1);
            buttons.push(b);
        }

        let lv_class = w("SysListView32");
        let list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            lv_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS,
            0,
            0,
            10,
            10,
            hwnd,
            IDC_LIST as HMENU,
            hinst,
            null(),
        );
        SendMessageW(list, WM_SETFONT, font as WPARAM, 1);
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            ex as WPARAM,
            ex as LPARAM,
        );
        for (i, (name, width, right)) in COLUMNS.iter().enumerate() {
            let mut text = w(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            col.cx = scale(*width, dpi);
            col.pszText = text.as_mut_ptr();
            col.iSubItem = i as i32;
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as LPARAM);
        }

        let sb_class = w("msctls_statusbar32");
        let status = CreateWindowExW(
            0,
            sb_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SBARS_SIZEGRIP,
            0,
            0,
            0,
            0,
            hwnd,
            IDC_STATUS as HMENU,
            hinst,
            null(),
        );
        SendMessageW(status, WM_SETFONT, font as WPARAM, 1);

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                session,
                list,
                status,
                buttons,
                rows: Vec::new(),
                dpi,
                ticks: 0,
            });
        });
    }
}

unsafe fn layout(hwnd: HWND) {
    let Some((list, status, buttons, dpi)) =
        with_app(|a| (a.list, a.status, a.buttons.clone(), a.dpi))
    else {
        return;
    };
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        SendMessageW(status, WM_SIZE, 0, 0);
        let mut src: RECT = std::mem::zeroed();
        GetWindowRect(status, &mut src);
        let sh = src.bottom - src.top;
        let parts = [scale(220, dpi), scale(330, dpi), scale(470, dpi), -1];
        SendMessageW(status, SB_SETPARTS, parts.len(), parts.as_ptr() as LPARAM);

        let pad = scale(6, dpi);
        let bh = scale(28, dpi);
        let mut x = pad;
        for (b, (_, _, width)) in buttons.iter().zip(BUTTONS) {
            let bw = scale(width, dpi);
            MoveWindow(*b, x, pad, bw, bh, 1);
            x += bw + scale(4, dpi);
        }
        let top = pad * 2 + bh;
        MoveWindow(list, 0, top, rc.right, (rc.bottom - top - sh).max(0), 1);
    }
}

unsafe fn tick(hwnd: HWND) {
    let Some(session) = with_app(|a| a.session.clone()) else {
        return;
    };
    let stats = session.stats();
    let rows: Vec<Row> = stats.iter().map(make_row).collect();
    let (down, up): (u64, u64) = stats
        .iter()
        .fold((0, 0), |(d, u), s| (d + s.down_rate, u + s.up_rate));
    let Some((list, status, count_changed, changed, save_now)) = with_app(|a| {
        let count_changed = a.rows.len() != rows.len();
        let changed: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| a.rows.get(*i) != Some(r))
            .map(|(i, _)| i)
            .collect();
        a.rows = rows;
        a.ticks += 1;
        (a.list, a.status, count_changed, changed, a.ticks % 30 == 0)
    }) else {
        return;
    };
    unsafe {
        if count_changed {
            let n = with_app(|a| a.rows.len()).unwrap_or(0);
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
        let parts = [
            format!(
                "\u{2193} {}   \u{2191} {}",
                fmt_or_zero(down),
                fmt_or_zero(up)
            ),
            match session.dht_nodes() {
                Some(n) => format!("DHT: {} nodes", n),
                None => "DHT: off".into(),
            },
            format!("Port: {}", session.port()),
            format!(
                "Saving to: {}",
                session
                    .download_dir
                    .lock()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            ),
        ];
        for (i, p) in parts.iter().enumerate() {
            let t = w(p);
            SendMessageW(status, SB_SETTEXTW, i, t.as_ptr() as LPARAM);
        }
        let title = if down > 0 || up > 0 {
            format!(
                "rustorrent  \u{2193} {}  \u{2191} {}",
                fmt_or_zero(down),
                fmt_or_zero(up)
            )
        } else {
            "rustorrent".to_string()
        };
        let t = w(&title);
        SetWindowTextW(hwnd, t.as_ptr());
    }
    if save_now && session.any_dirty() {
        session.save();
    }
}

fn fmt_or_zero(n: u64) -> String {
    if n == 0 { "0 B/s".into() } else { fmt_rate(n) }
}

unsafe fn selected_ids() -> Vec<u64> {
    let Some(list) = with_app(|a| a.list) else {
        return Vec::new();
    };
    let mut idx: isize = -1;
    let mut out = Vec::new();
    loop {
        let r =
            unsafe { SendMessageW(list, LVM_GETNEXTITEM, idx as usize, LVNI_SELECTED as LPARAM) };
        if r < 0 || out.len() > 100_000 {
            break;
        }
        idx = r;
        out.push(r as usize);
    }
    with_app(|a| {
        out.iter()
            .filter_map(|&i| a.rows.get(i).map(|r| r.id))
            .collect()
    })
    .unwrap_or_default()
}

unsafe fn message(hwnd: HWND, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let t = w(text);
    let c = w("rustorrent");
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), flags) }
}

unsafe fn add_item(hwnd: HWND, item: &str) {
    let Some(session) = with_app(|a| a.session.clone()) else {
        return;
    };
    let item = item.trim().trim_matches('"');
    if item.is_empty() {
        return;
    }
    let res = if item.to_ascii_lowercase().starts_with("magnet:") {
        session.add_magnet(item)
    } else {
        session.add_torrent_file(std::path::Path::new(item))
    };
    if let Err(e) = res {
        unsafe {
            message(
                hwnd,
                &format!("Could not add:\n{}\n\n{}", item, e),
                MB_OK | MB_ICONWARNING,
            )
        };
    }
    unsafe { tick(hwnd) };
}

unsafe fn add_lines(hwnd: HWND, text: &str) {
    for line in text.lines() {
        if !line.trim().is_empty() {
            unsafe { add_item(hwnd, line) };
        }
    }
}

unsafe fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(CF_UNICODETEXT);
        if !h.is_null() {
            let p = GlobalLock(h) as *const u16;
            if !p.is_null() {
                let max = GlobalSize(h) / 2;
                let mut n = 0;
                while n < max && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        out
    }
}

unsafe fn set_clipboard(hwnd: HWND, text: &str) {
    let data = w(text);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
                GlobalUnlock(h);
                if SetClipboardData(CF_UNICODETEXT, h).is_null() {
                    GlobalFree(h);
                }
            }
        }
        CloseClipboard();
    }
}

unsafe fn open_torrent_dialog(hwnd: HWND) -> Vec<PathBuf> {
    let mut buf = vec![0u16; 65536];
    let filter: Vec<u16> = "Torrent files (*.torrent)\0*.torrent\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = w("Add torrent");
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_ALLOWMULTISELECT
        | OFN_EXPLORER
        | OFN_FILEMUSTEXIST
        | OFN_PATHMUSTEXIST
        | OFN_HIDEREADONLY;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return Vec::new();
    }
    // Either "full path\0\0" or "dir\0file1\0file2\0\0".
    let parts: Vec<String> = buf
        .split(|&c| c == 0)
        .take_while(|p| !p.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.len() {
        0 => Vec::new(),
        1 => vec![PathBuf::from(&parts[0])],
        _ => parts[1..]
            .iter()
            .map(|f| PathBuf::from(&parts[0]).join(f))
            .collect(),
    }
}

unsafe fn browse_folder(hwnd: HWND) -> Option<PathBuf> {
    let title = w("Choose the folder where new downloads are saved");
    let mut bi: BROWSEINFOW = unsafe { std::mem::zeroed() };
    bi.hwndOwner = hwnd;
    bi.lpszTitle = title.as_ptr();
    bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
    unsafe {
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }
        let mut path = vec![0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr()) != 0;
        CoTaskMemFree(pidl as *const c_void);
        if !ok {
            return None;
        }
        let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        Some(PathBuf::from(String::from_utf16_lossy(&path[..n])))
    }
}

unsafe fn open_folder(id: u64) {
    let Some(session) = with_app(|a| a.session.clone()) else {
        return;
    };
    let Some(path) = session.content_path(id) else {
        return;
    };
    let explorer = w("explorer.exe");
    let open = w("open");
    let params = if path.exists() {
        w(&format!("/select,\"{}\"", path.display()))
    } else {
        w(&format!(
            "\"{}\"",
            path.parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ))
    };
    unsafe {
        ShellExecuteW(
            null_mut(),
            open.as_ptr(),
            explorer.as_ptr(),
            params.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    };
}

unsafe fn clear_selection(list: HWND) {
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.stateMask = LVIS_SELECTED;
    item.state = 0;
    unsafe {
        SendMessageW(
            list,
            LVM_SETITEMSTATE,
            usize::MAX,
            &item as *const _ as LPARAM,
        )
    };
}

unsafe fn remove_selected(hwnd: HWND, delete: bool) {
    let ids = unsafe { selected_ids() };
    if ids.is_empty() {
        return;
    }
    let n = ids.len();
    let what = if n == 1 {
        "this torrent".to_string()
    } else {
        format!("these {} torrents", n)
    };
    let (text, flags) = if delete {
        (
            format!(
                "Remove {} and permanently DELETE the downloaded files?",
                what
            ),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    } else {
        (
            format!(
                "Remove {} from the list?\n\nDownloaded files are kept.",
                what
            ),
            MB_YESNO | MB_ICONQUESTION,
        )
    };
    if unsafe { message(hwnd, &text, flags) } != IDYES {
        return;
    }
    let Some((session, list)) = with_app(|a| (a.session.clone(), a.list)) else {
        return;
    };
    unsafe { clear_selection(list) };
    for id in ids {
        session.remove(id, delete);
    }
    unsafe { tick(hwnd) };
}

unsafe fn command(hwnd: HWND, id: usize) {
    let Some(session) = with_app(|a| a.session.clone()) else {
        return;
    };
    unsafe {
        match id {
            ID_ADD => {
                for p in open_torrent_dialog(hwnd) {
                    add_item(hwnd, &p.to_string_lossy());
                }
            }
            ID_MAGNET => {
                if let Some(text) = input_dialog(hwnd) {
                    add_lines(hwnd, &text);
                }
            }
            ID_START => {
                for id in selected_ids() {
                    session.resume(id);
                }
            }
            ID_PAUSE => {
                for id in selected_ids() {
                    session.pause(id);
                }
            }
            ID_RECHECK => {
                for id in selected_ids() {
                    session.recheck(id);
                }
            }
            ID_REMOVE => remove_selected(hwnd, false),
            ID_REMOVE_DELETE => remove_selected(hwnd, true),
            ID_OPEN => {
                if let Some(&id) = selected_ids().first() {
                    open_folder(id);
                }
            }
            ID_COPY_MAGNET => {
                let links: Vec<String> = selected_ids()
                    .into_iter()
                    .filter_map(|id| session.magnet_link(id))
                    .collect();
                if !links.is_empty() {
                    set_clipboard(hwnd, &links.join("\r\n"));
                }
            }
            ID_SETDIR => {
                if let Some(dir) = browse_folder(hwnd) {
                    if let Ok(mut d) = session.download_dir.lock() {
                        *d = dir;
                    }
                    session.save();
                }
            }
            _ => return,
        }
        tick(hwnd);
    }
}

unsafe fn context_menu(hwnd: HWND) {
    let ids = unsafe { selected_ids() };
    if ids.is_empty() {
        return;
    }
    unsafe {
        let menu = CreatePopupMenu();
        let items: [(usize, &str); 9] = [
            (ID_START, "Start"),
            (ID_PAUSE, "Pause"),
            (0, ""),
            (ID_OPEN, "Open folder"),
            (ID_COPY_MAGNET, "Copy magnet link"),
            (ID_RECHECK, "Force recheck"),
            (0, ""),
            (ID_REMOVE, "Remove\tDel"),
            (ID_REMOVE_DELETE, "Remove and delete files\tShift+Del"),
        ];
        for (id, text) in items {
            if id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            } else {
                let t = w(text);
                AppendMenuW(menu, MF_STRING, id, t.as_ptr());
            }
        }
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
        DestroyMenu(menu);
    }
}

unsafe fn draw_progress(cd: &NMLVCUSTOMDRAW, list: HWND) {
    let item = cd.nmcd.dwItemSpec;
    let Some((progress, color, text)) = with_app(|a| {
        a.rows
            .get(item)
            .map(|r| (r.progress, r.color, r.cells[PROGRESS_COL as usize].clone()))
    })
    .flatten() else {
        return;
    };
    unsafe {
        let mut rc = RECT {
            left: LVIR_BOUNDS as i32,
            top: PROGRESS_COL,
            right: 0,
            bottom: 0,
        };
        SendMessageW(list, LVM_GETSUBITEMRECT, item, &mut rc as *mut _ as LPARAM);
        let hdc = cd.nmcd.hdc;
        FillRect(hdc, &rc, GetSysColorBrush(COLOR_WINDOW));
        let mut bar = RECT {
            left: rc.left + 3,
            top: rc.top + 2,
            right: rc.right - 3,
            bottom: rc.bottom - 2,
        };
        let frame = CreateSolidBrush(rgb(200, 200, 200));
        FrameRect(hdc, &bar, frame);
        DeleteObject(frame as HGDIOBJ);
        bar.left += 1;
        bar.top += 1;
        bar.bottom -= 1;
        let inner_w = (bar.right - 1 - bar.left).max(0);
        let mut fill = bar;
        fill.right = fill.left + (inner_w as f64 * progress.clamp(0.0, 1.0)) as i32;
        let brush = CreateSolidBrush(color);
        FillRect(hdc, &fill, brush);
        DeleteObject(brush as HGDIOBJ);
        SetBkMode(hdc, TRANSPARENT as _);
        SetTextColor(hdc, rgb(20, 20, 20));
        let mut t: Vec<u16> = text.encode_utf16().collect();
        DrawTextW(
            hdc,
            t.as_mut_ptr(),
            t.len() as i32,
            &mut bar,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
    let hdr = unsafe { &*(lparam as *const NMHDR) };
    if hdr.idFrom != IDC_LIST {
        return None;
    }
    unsafe {
        match hdr.code {
            LVN_GETDISPINFOW => {
                let di = &mut *(lparam as *mut NMLVDISPINFOW);
                if di.item.mask & LVIF_TEXT != 0
                    && !di.item.pszText.is_null()
                    && di.item.cchTextMax > 0
                {
                    let text = with_app(|a| {
                        a.rows
                            .get(di.item.iItem as usize)
                            .and_then(|r| r.cells.get(di.item.iSubItem as usize))
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
                Some(0)
            }
            NM_CUSTOMDRAW => {
                let cd = &*(lparam as *const NMLVCUSTOMDRAW);
                let stage = cd.nmcd.dwDrawStage;
                if stage == CDDS_PREPAINT {
                    Some(CDRF_NOTIFYITEMDRAW as LRESULT)
                } else if stage == CDDS_ITEMPREPAINT {
                    Some(CDRF_NOTIFYSUBITEMDRAW as LRESULT)
                } else if stage == (CDDS_ITEMPREPAINT | CDDS_SUBITEM) && cd.iSubItem == PROGRESS_COL
                {
                    draw_progress(cd, hdr.hwndFrom);
                    Some(CDRF_SKIPDEFAULT as LRESULT)
                } else {
                    Some(CDRF_DODEFAULT as LRESULT)
                }
            }
            NM_RCLICK => {
                context_menu(hwnd);
                Some(0)
            }
            NM_DBLCLK => {
                if let Some(&id) = selected_ids().first() {
                    open_folder(id);
                }
                Some(0)
            }
            LVN_KEYDOWN => {
                let kd = &*(lparam as *const NMLVKEYDOWN);
                if kd.wVKey == VK_DELETE {
                    remove_selected(hwnd, GetKeyState(VK_SHIFT as i32) < 0);
                } else if kd.wVKey == b'A' as u16 && GetKeyState(VK_CONTROL as i32) < 0 {
                    let mut item: LVITEMW = std::mem::zeroed();
                    item.stateMask = LVIS_SELECTED;
                    item.state = LVIS_SELECTED;
                    SendMessageW(
                        hdr.hwndFrom,
                        LVM_SETITEMSTATE,
                        usize::MAX,
                        &item as *const _ as LPARAM,
                    );
                }
                Some(0)
            }
            _ => None,
        }
    }
}

unsafe extern "system" fn main_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_SIZE => {
                layout(hwnd);
                0
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam as *mut MINMAXINFO);
                mmi.ptMinTrackSize = POINT { x: 640, y: 300 };
                0
            }
            WM_TIMER => {
                tick(hwnd);
                0
            }
            WM_COMMAND => {
                command(hwnd, wparam & 0xffff);
                0
            }
            WM_NOTIFY => match on_notify(hwnd, lparam) {
                Some(r) => r,
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_DROPFILES => {
                let hdrop = wparam as HDROP;
                let n = DragQueryFileW(hdrop, u32::MAX, null_mut(), 0);
                let mut files = Vec::new();
                for i in 0..n {
                    let len = DragQueryFileW(hdrop, i, null_mut(), 0) as usize;
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32);
                    files.push(String::from_utf16_lossy(&buf[..len]));
                }
                DragFinish(hdrop);
                for f in files {
                    add_item(hwnd, &f);
                }
                0
            }
            WM_COPYDATA => {
                let cds = &*(lparam as *const COPYDATASTRUCT);
                if cds.dwData == 1 && !cds.lpData.is_null() && cds.cbData < 1 << 20 {
                    let s = std::slice::from_raw_parts(
                        cds.lpData as *const u16,
                        cds.cbData as usize / 2,
                    );
                    let text = String::from_utf16_lossy(s);
                    add_item(hwnd, &text);
                }
                1
            }
            WM_CLOSE => {
                KillTimer(hwnd, TIMER_ID);
                if let Some(s) = with_app(|a| a.session.clone()) {
                    s.shutdown();
                }
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ---- magnet input window ----

unsafe extern "system" fn input_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wparam & 0xffff) as i32;
                if id == IDOK {
                    let edit = GetDlgItem(hwnd, IDC_INPUT_EDIT as i32);
                    let len = GetWindowTextLengthW(edit).max(0) as usize;
                    let mut buf = vec![0u16; len + 1];
                    GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
                    INPUT_TEXT
                        .with(|t| *t.borrow_mut() = Some(String::from_utf16_lossy(&buf[..len])));
                    INPUT_DONE.with(|d| d.set(true));
                } else if id == IDCANCEL {
                    INPUT_DONE.with(|d| d.set(true));
                }
                0
            }
            WM_CLOSE => {
                INPUT_DONE.with(|d| d.set(true));
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn input_dialog(parent: HWND) -> Option<String> {
    let dpi = with_app(|a| a.dpi).unwrap_or(96);
    let s = |v| scale(v, dpi);
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = w(INPUT_CLASS);
        let title = w("Add magnet links");
        let (width, height) = (s(560), s(250));
        let mut prc: RECT = std::mem::zeroed();
        GetWindowRect(parent, &mut prc);
        let x = prc.left + ((prc.right - prc.left) - width) / 2;
        let y = prc.top + ((prc.bottom - prc.top) - height) / 2;
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            width,
            height,
            parent,
            null_mut(),
            hinst,
            null(),
        );
        if dlg.is_null() {
            return None;
        }
        let mut crc: RECT = std::mem::zeroed();
        GetClientRect(dlg, &mut crc);
        let font = SendMessageW(parent, WM_GETFONT, 0, 0);
        let font = if font == 0 {
            with_app(|a| SendMessageW(a.list, WM_GETFONT, 0, 0)).unwrap_or(0)
        } else {
            font
        };
        let pad = s(10);
        let label_class = w("STATIC");
        let label_text = w("Paste one or more magnet links (one per line):");
        let label = CreateWindowExW(
            0,
            label_class.as_ptr(),
            label_text.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            pad,
            pad,
            crc.right - 2 * pad,
            s(18),
            dlg,
            null_mut(),
            hinst,
            null(),
        );
        let edit_class = w("EDIT");
        let initial = clipboard_text(parent)
            .filter(|t| t.trim_start().to_ascii_lowercase().starts_with("magnet:"))
            .unwrap_or_default();
        let initial = w(&initial.replace("\r\n", "\n").replace('\n', "\r\n"));
        let bh = s(28);
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            initial.as_ptr(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL) as u32,
            pad,
            pad + s(22),
            crc.right - 2 * pad,
            crc.bottom - 3 * pad - s(22) - bh,
            dlg,
            IDC_INPUT_EDIT as HMENU,
            hinst,
            null(),
        );
        let button_class = w("BUTTON");
        let ok_text = w("Add");
        let cancel_text = w("Cancel");
        let bw = s(88);
        let ok = CreateWindowExW(
            0,
            button_class.as_ptr(),
            ok_text.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            crc.right - pad - 2 * bw - s(6),
            crc.bottom - pad - bh,
            bw,
            bh,
            dlg,
            IDOK as usize as HMENU,
            hinst,
            null(),
        );
        let cancel = CreateWindowExW(
            0,
            button_class.as_ptr(),
            cancel_text.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            crc.right - pad - bw,
            crc.bottom - pad - bh,
            bw,
            bh,
            dlg,
            IDCANCEL as usize as HMENU,
            hinst,
            null(),
        );
        for c in [label, edit, ok, cancel] {
            SendMessageW(c, WM_SETFONT, font as WPARAM, 1);
        }
        SendMessageW(edit, EM_SETSEL, 0, -1);

        INPUT_DONE.with(|d| d.set(false));
        INPUT_TEXT.with(|t| *t.borrow_mut() = None);
        EnableWindow(parent, 0);
        ShowWindow(dlg, SW_SHOW);
        SetFocus(edit);
        let mut msg: MSG = std::mem::zeroed();
        let mut quit = false;
        while !INPUT_DONE.with(|d| d.get()) {
            let r = GetMessageW(&mut msg, null_mut(), 0, 0);
            if r <= 0 {
                quit = true;
                break;
            }
            // Ctrl+Enter in the edit box confirms, like Enter does for single-line input.
            if msg.message == WM_KEYDOWN && msg.wParam == VK_RETURN as usize && msg.hwnd == edit {
                SendMessageW(dlg, WM_COMMAND, IDOK as usize, 0);
                continue;
            }
            if IsDialogMessageW(dlg, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        EnableWindow(parent, 1);
        SetForegroundWindow(parent);
        DestroyWindow(dlg);
        if quit {
            PostQuitMessage(0);
        }
        INPUT_TEXT.with(|t| t.borrow_mut().take())
    }
}
