//! Native Win32 user interface (no GUI framework: keeps the executable small).

mod details;
mod dialogs;

use crate::metainfo::Metainfo;
use crate::session::{Config, Session};
use crate::torrent::{Stats, Status};
use crate::util::{fmt_bytes, fmt_eta, fmt_rate};
use std::cell::RefCell;
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
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const MAIN_CLASS: &str = "RustorrentMainWindow";
const CF_UNICODETEXT: u32 = 13;

const IDC_LIST: usize = 100;
const IDC_STATUS: usize = 101;

const ID_ADD: usize = 201;
const ID_MAGNET: usize = 202;
const ID_START: usize = 203;
const ID_PAUSE: usize = 204;
const ID_REMOVE: usize = 205;
const ID_OPEN: usize = 206;
const ID_SETTINGS: usize = 207;
const ID_REMOVE_DELETE: usize = 208;
const ID_COPY_MAGNET: usize = 209;
const ID_RECHECK: usize = 210;
const ID_LIMIT: usize = 211;
const ID_REANNOUNCE: usize = 212;
const TIMER_ID: usize = 1;

const BUTTONS: [(&str, usize, i32); 7] = [
    ("Add torrent\u{2026}", ID_ADD, 104),
    ("Add magnet\u{2026}", ID_MAGNET, 104),
    ("Start", ID_START, 64),
    ("Pause", ID_PAUSE, 64),
    ("Remove", ID_REMOVE, 72),
    ("Open folder", ID_OPEN, 92),
    ("Settings\u{2026}", ID_SETTINGS, 88),
];
/// Height of the draggable bar between the torrent list and the details panel.
const SPLITTER: i32 = 6;

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
    font: WPARAM,
    ticks: u32,
    panel: details::Panel,
    /// Fraction of the available height given to the torrent list.
    split: f32,
    dragging: bool,
    /// Y range of the splitter bar in client coordinates.
    splitter_y: (i32, i32),
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
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
    install_panic_hook();
    crate::log(&format!(
        "starting rustorrent {} (data folder: {})",
        env!("CARGO_PKG_VERSION"),
        crate::data_dir().display()
    ));
    unsafe {
        let class = w(MAIN_CLASS);
        // Single instance: hand arguments to the running window and leave.
        if hand_over_to_running(&class, &args) {
            return;
        }
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES | ICC_TAB_CLASSES,
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
        if RegisterClassExW(&wc) == 0 {
            crate::log(&format!(
                "RegisterClassExW failed: error {}",
                GetLastError()
            ));
        }
        let dlg_class = w(dialogs::DLG_CLASS);
        wc.lpfnWndProc = Some(dialogs::dlg_proc);
        wc.lpszClassName = dlg_class.as_ptr();
        RegisterClassExW(&wc);
        let general_class = w(details::GENERAL_CLASS);
        wc.lpfnWndProc = Some(details::general_proc);
        wc.lpszClassName = general_class.as_ptr();
        wc.hbrBackground = null_mut();
        RegisterClassExW(&wc);

        let hdc = GetDC(null_mut());
        let dpi = GetDeviceCaps(hdc, LOGPIXELSY as _).max(96);
        ReleaseDC(null_mut(), hdc);

        let title = w("rustorrent (starting\u{2026})");
        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(1060, dpi),
            scale(680, dpi),
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            let err = GetLastError();
            crate::log(&format!("CreateWindowExW failed: error {}", err));
            message(
                null_mut(),
                &format!("rustorrent could not create its window (error {}).", err),
                MB_OK | MB_ICONERROR,
            );
            return;
        }
        // Show the window before starting the engine, so slow disks or networks
        // (port forwarding, DHT, resuming torrents) can never keep it hidden.
        keep_on_screen(hwnd);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);
        crate::log("window shown");

        let Some(session) = start_session(hwnd) else {
            crate::log("closed before the engine finished starting");
            return;
        };
        create_children(hwnd, hinst, session, dpi);
        layout(hwnd);
        DragAcceptFiles(hwnd, 1);
        SetTimer(hwnd, TIMER_ID, 1000, None);
        for a in &args {
            add_item(hwnd, a);
        }
        tick(hwnd);
        crate::log("ready");

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
        crate::log("exiting");
    }
}

/// Log panics and tell the user, instead of vanishing silently (there is no console).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let text = format!("panic: {}", info);
        crate::log(&text);
        unsafe {
            message(
                null_mut(),
                &format!(
                    "rustorrent hit an internal error and has to close.\n\n{}\n\nDetails were saved to {}",
                    text,
                    crate::data_dir().join("rustorrent.log").display()
                ),
                MB_OK | MB_ICONERROR,
            );
        }
    }));
}

/// If rustorrent is already running, pass it our arguments and bring it forward.
/// Returns true when this process should exit.
unsafe fn hand_over_to_running(class: &[u16], args: &[String]) -> bool {
    unsafe {
        let existing = FindWindowW(class.as_ptr(), null());
        if existing.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(existing, &mut pid);
        let mut res = 0usize;
        let responding =
            SendMessageTimeoutW(existing, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 3000, &mut res) != 0;
        if responding {
            crate::log(&format!("already running as process {}; handing over", pid));
            for a in args {
                send_copydata(existing, a);
            }
            if IsWindowVisible(existing) == 0 {
                ShowWindow(existing, SW_SHOW);
            }
            if IsIconic(existing) != 0 {
                ShowWindow(existing, SW_RESTORE);
            }
            SetForegroundWindow(existing);
            return true;
        }
        // An earlier copy is stuck. Exiting quietly here would look like
        // "nothing happens" to the user, so ask what to do instead.
        crate::log(&format!(
            "process {} has a window but is not responding",
            pid
        ));
        let text = format!(
            "Another copy of rustorrent (process {}) is running but not responding.\n\nEnd it and start a new one?",
            pid
        );
        if message(null_mut(), &text, MB_YESNO | MB_ICONWARNING) != IDYES {
            return true;
        }
        let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() || TerminateProcess(h, 1) == 0 {
            if !h.is_null() {
                CloseHandle(h);
            }
            message(
                null_mut(),
                &format!(
                    "Could not end process {}. Please end rustorrent.exe in Task Manager and try again.",
                    pid
                ),
                MB_OK | MB_ICONERROR,
            );
            return true;
        }
        WaitForSingleObject(h, 5000);
        CloseHandle(h);
        crate::log(&format!("ended stuck process {}", pid));
        false
    }
}

/// Move the window onto the primary work area if it is not on any monitor.
unsafe fn keep_on_screen(hwnd: HWND) {
    unsafe {
        if !MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL).is_null() {
            return;
        }
        let mut wa: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut c_void, 0);
        let mut rc: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut rc);
        let width = (rc.right - rc.left).min(wa.right - wa.left);
        let height = (rc.bottom - rc.top).min(wa.bottom - wa.top);
        crate::log("window was off-screen; moving it to the primary monitor");
        SetWindowPos(
            hwnd,
            null_mut(),
            wa.left + (wa.right - wa.left - width) / 2,
            wa.top + (wa.bottom - wa.top - height) / 2,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Start the engine on a worker thread while the (already visible) window keeps
/// handling messages. Returns None if the user closed the window meanwhile.
unsafe fn start_session(hwnd: HWND) -> Option<Arc<Session>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let s = Session::new(Config {
            data_dir: crate::data_dir(),
            port: 0,
            dht: true,
            persist: true,
            download_dir: crate::default_download_dir(),
        });
        let _ = tx.send(s);
    });
    let started = std::time::Instant::now();
    let mut warned = false;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(s) => {
                crate::log(&format!(
                    "engine started in {} ms (port {})",
                    started.elapsed().as_millis(),
                    s.port()
                ));
                return Some(s);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The panic hook has already logged and reported it.
                unsafe { DestroyWindow(hwnd) };
                return None;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !warned && started.elapsed() > std::time::Duration::from_secs(10) {
            warned = true;
            crate::log("engine is taking more than 10 s to start");
        }
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    return None;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
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
        let panel = details::create(hwnd, hinst, font as WPARAM, dpi);

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                session,
                list,
                status,
                buttons,
                rows: Vec::new(),
                dpi,
                font: font as WPARAM,
                ticks: 0,
                panel,
                split: 0.5,
                dragging: false,
                splitter_y: (0, 0),
            });
        });
    }
}

unsafe fn layout(hwnd: HWND) {
    let Some((list, status, buttons, dpi, split)) =
        with_app(|a| (a.list, a.status, a.buttons.clone(), a.dpi, a.split))
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
        let parts = [scale(280, dpi), scale(390, dpi), scale(720, dpi), -1];
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
        let avail = (rc.bottom - top - sh).max(0);
        let gap = scale(SPLITTER, dpi);
        let min_list = scale(80, dpi).min(avail / 2);
        let min_panel = scale(140, dpi).min(avail / 2);
        let list_h = ((avail as f32 * split) as i32)
            .clamp(min_list, (avail - gap - min_panel).max(min_list));
        MoveWindow(list, 0, top, rc.right, list_h, 1);
        let panel_y = top + list_h + gap;
        with_app(|a| a.splitter_y = (top + list_h, panel_y));
        details::layout(0, panel_y, rc.right, (rc.bottom - sh - panel_y).max(0));
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
        let (dl, ul) = session.global_limits();
        let with_limit = |rate: u64, limit: u64| {
            if limit > 0 {
                format!("{} [{}]", fmt_or_zero(rate), fmt_rate(limit))
            } else {
                fmt_or_zero(rate)
            }
        };
        let parts = [
            format!(
                "\u{2193} {}   \u{2191} {}",
                with_limit(down, dl),
                with_limit(up, ul)
            ),
            match session.dht_nodes() {
                Some(n) => format!("DHT: {} nodes", n),
                None => "DHT: off".into(),
            },
            match session.portmap_status() {
                Some(pm) => format!("Port {}: {}", session.port(), pm),
                None => format!("Port: {}", session.port()),
            },
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
        details::refresh(hwnd);
    }
    if save_now && session.any_dirty() {
        session.save();
    }
}

fn fmt_or_zero(n: u64) -> String {
    if n == 0 { "0 B/s".into() } else { fmt_rate(n) }
}

/// Indices of the selected rows of a list view.
unsafe fn selected_rows(list: HWND) -> Vec<usize> {
    let mut idx: isize = -1;
    let mut out = Vec::new();
    loop {
        let r =
            unsafe { SendMessageW(list, LVM_GETNEXTITEM, idx as usize, LVNI_SELECTED as LPARAM) };
        if r < 0 || out.len() > 1_000_000 {
            break;
        }
        idx = r;
        out.push(r as usize);
    }
    out
}

unsafe fn selected_ids() -> Vec<u64> {
    let Some(list) = with_app(|a| a.list) else {
        return Vec::new();
    };
    let out = unsafe { selected_rows(list) };
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
    } else if session.settings().ask_files {
        unsafe { add_torrent_interactive(hwnd, &session, std::path::Path::new(item)) }
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

/// Show the file selection dialog for a .torrent before adding it.
unsafe fn add_torrent_interactive(
    hwnd: HWND,
    session: &Session,
    path: &std::path::Path,
) -> crate::util::Result<u64> {
    let len = std::fs::metadata(path)?.len();
    if len > 64 << 20 {
        crate::bail!("file is too large to be a torrent");
    }
    let data = std::fs::read(path)?;
    let (meta, _) = Metainfo::from_torrent(&data)?;
    if session.contains(&meta.info_hash) {
        return session.add_torrent_bytes(&data);
    }
    let dir = session.download_dir.lock().unwrap().clone();
    match unsafe { dialogs::add_torrent(hwnd, &meta, &dir) } {
        Some(opts) => session.add_torrent_with(&data, opts),
        None => Ok(0),
    }
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
                let initial = clipboard_text(hwnd)
                    .filter(|t| t.trim_start().to_ascii_lowercase().starts_with("magnet:"))
                    .unwrap_or_default();
                if let Some(text) = dialogs::text_input(
                    hwnd,
                    "Add magnet links",
                    "Paste one or more magnet links (one per line):",
                    &initial,
                    "Add",
                ) {
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
            ID_SETTINGS => {
                let cur = session.settings();
                if let Some(new) = dialogs::settings(hwnd, &cur, session.port()) {
                    if new != cur && session.apply_settings(new) {
                        message(
                            hwnd,
                            "Some changes (port or DHT) will take effect the next time rustorrent starts.",
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                }
            }
            ID_LIMIT => {
                let ids = selected_ids();
                if let Some(&first) = ids.first() {
                    let cur = session.torrent_limits(first).unwrap_or((0, 0));
                    if let Some((d, u)) = dialogs::speed_limit(hwnd, ids.len(), cur) {
                        for id in ids {
                            session.set_torrent_limits(id, d, u);
                        }
                    }
                }
            }
            ID_REANNOUNCE => {
                for id in selected_ids() {
                    session.reannounce(id);
                }
            }
            _ => {
                if !details::command(hwnd, id) {
                    return;
                }
            }
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
        popup_menu(
            hwnd,
            &[
                (ID_START, "Start"),
                (ID_PAUSE, "Pause"),
                (0, ""),
                (ID_OPEN, "Open folder"),
                (ID_COPY_MAGNET, "Copy magnet link"),
                (ID_LIMIT, "Speed limit\u{2026}"),
                (ID_RECHECK, "Force recheck"),
                (ID_REANNOUNCE, "Force reannounce"),
                (0, ""),
                (ID_REMOVE, "Remove\tDel"),
                (ID_REMOVE_DELETE, "Remove and delete files\tShift+Del"),
            ],
        );
    }
}

unsafe fn popup_menu(hwnd: HWND, items: &[(usize, &str)]) {
    unsafe {
        let menu = CreatePopupMenu();
        for &(id, text) in items {
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
        draw_bar(
            cd.nmcd.hdc,
            list,
            item,
            PROGRESS_COL,
            progress,
            color,
            &text,
        )
    };
}

/// Draw a progress bar with centred text into a list-view cell.
unsafe fn draw_bar(
    hdc: HDC,
    list: HWND,
    item: usize,
    col: i32,
    progress: f64,
    color: COLORREF,
    text: &str,
) {
    unsafe {
        let mut rc = RECT {
            left: LVIR_BOUNDS as i32,
            top: col,
            right: 0,
            bottom: 0,
        };
        SendMessageW(list, LVM_GETSUBITEMRECT, item, &mut rc as *mut _ as LPARAM);
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

/// Notifications from the details panel (tab control and its lists).
unsafe fn on_panel_notify(hwnd: HWND, hdr: &NMHDR, lparam: LPARAM) -> Option<LRESULT> {
    unsafe {
        if hdr.idFrom == details::IDC_TAB {
            if hdr.code == TCN_SELCHANGE {
                details::tab_changed(hwnd);
            }
            return Some(0);
        }
        let which = with_app(|a| a.panel.list_index(hdr.hwndFrom)).flatten()?;
        match hdr.code {
            LVN_GETDISPINFOW => {
                details::dispinfo(which, &mut *(lparam as *mut NMLVDISPINFOW));
                Some(0)
            }
            NM_CUSTOMDRAW if which == 2 => {
                let cd = &*(lparam as *const NMLVCUSTOMDRAW);
                let stage = cd.nmcd.dwDrawStage;
                Some(if stage == CDDS_PREPAINT {
                    CDRF_NOTIFYITEMDRAW as LRESULT
                } else if stage == CDDS_ITEMPREPAINT {
                    CDRF_NOTIFYSUBITEMDRAW as LRESULT
                } else if stage == (CDDS_ITEMPREPAINT | CDDS_SUBITEM)
                    && cd.iSubItem == details::FILE_PROGRESS_COL
                {
                    details::draw_file_progress(cd, hdr.hwndFrom);
                    CDRF_SKIPDEFAULT as LRESULT
                } else {
                    CDRF_DODEFAULT as LRESULT
                })
            }
            NM_RCLICK => {
                details::context_menu(hwnd, which);
                Some(0)
            }
            NM_CLICK if which == 2 => {
                details::on_files_click(hdr.hwndFrom, lparam);
                Some(0)
            }
            NM_DBLCLK if which == 2 => {
                details::command(hwnd, details::ID_FILE_OPEN);
                Some(0)
            }
            LVN_KEYDOWN if which == 2 => {
                let kd = &*(lparam as *const NMLVKEYDOWN);
                details::on_files_key(hdr.hwndFrom, kd.wVKey);
                Some(0)
            }
            _ => None,
        }
    }
}

unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
    let hdr = unsafe { &*(lparam as *const NMHDR) };
    if hdr.idFrom != IDC_LIST {
        return unsafe { on_panel_notify(hwnd, hdr, lparam) };
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
            LVN_ITEMCHANGED => {
                let nm = &*(lparam as *const NMLISTVIEW);
                if nm.uChanged & LVIF_STATE != 0
                    && (nm.uOldState ^ nm.uNewState) & LVIS_SELECTED != 0
                {
                    // Show the newly selected torrent right away (after the click settles).
                    PostMessageW(hwnd, WM_APP_REFRESH, 0, 0);
                }
                Some(0)
            }
            LVN_ODSTATECHANGED => {
                PostMessageW(hwnd, WM_APP_REFRESH, 0, 0);
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
            WM_APP_REFRESH => {
                // Collapse bursts of selection changes into one refresh.
                let mut m: MSG = std::mem::zeroed();
                while PeekMessageW(&mut m, hwnd, WM_APP_REFRESH, WM_APP_REFRESH, PM_REMOVE) != 0 {}
                with_app(|a| a.panel.rows = Default::default());
                details::refresh(hwnd);
                0
            }
            WM_SETCURSOR => {
                if (lparam & 0xffff) as u32 == HTCLIENT && over_splitter(hwnd) {
                    SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
                    1
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_LBUTTONDOWN => {
                if over_splitter(hwnd) {
                    with_app(|a| a.dragging = true);
                    SetCapture(hwnd);
                }
                0
            }
            WM_MOUSEMOVE => {
                if with_app(|a| a.dragging).unwrap_or(false) {
                    let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                    drag_splitter(hwnd, y);
                }
                0
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if with_app(|a| std::mem::replace(&mut a.dragging, false)).unwrap_or(false)
                    && msg == WM_LBUTTONUP
                {
                    ReleaseCapture();
                }
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
                crate::log("closing");
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

const WM_APP_REFRESH: u32 = WM_APP + 1;

unsafe fn over_splitter(hwnd: HWND) -> bool {
    unsafe {
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        ScreenToClient(hwnd, &mut pt);
        with_app(|a| pt.y >= a.splitter_y.0 && pt.y < a.splitter_y.1).unwrap_or(false)
    }
}

unsafe fn drag_splitter(hwnd: HWND, y: i32) {
    let Some((list, status)) = with_app(|a| (a.list, a.status)) else {
        return;
    };
    unsafe {
        let mut lr: RECT = std::mem::zeroed();
        GetWindowRect(list, &mut lr);
        let mut top = POINT { x: 0, y: lr.top };
        ScreenToClient(hwnd, &mut top);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        let mut src: RECT = std::mem::zeroed();
        GetWindowRect(status, &mut src);
        let avail = (rc.bottom - top.y - (src.bottom - src.top)).max(1);
        let split = ((y - top.y) as f32 / avail as f32).clamp(0.1, 0.9);
        with_app(|a| a.split = split);
        layout(hwnd);
    }
}
