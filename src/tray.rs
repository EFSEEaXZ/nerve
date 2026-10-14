//! Windows tray icon: state colour (green online / amber starting / red error), tooltip with the
//! public URL, and a menu (dashboard, copy URL, restart tunnel/server, open data folder).

use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::control::Quit;
use crate::server::AppState;

const WM_TRAY: u32 = WM_APP + 1;
const ID_DASH: usize = 1;
const ID_COPY: usize = 2;
const ID_TUNNEL: usize = 3;
const ID_RESTART: usize = 4;
const ID_FOLDER: usize = 5;
const ID_LOGS: usize = 6;
const ID_UPDATE: usize = 7;
static mut POPUP_OPEN: bool = false;
static mut SELECTED: i32 = 0;
static mut SCALE: f32 = 1.0;
static mut LIGHT: bool = false;
const ITEMS: &[(usize, &str)] = &[
    (ID_DASH, "Открыть панель"),
    (ID_COPY, "Копировать MCP URL"),
    (ID_UPDATE, "Проверить обновления"),
    (ID_TUNNEL, "Перезапустить туннель"),
    (ID_RESTART, "Перезапустить nerve"),
    (ID_LOGS, "Папка логов"),
    (ID_FOLDER, "Папка данных"),
];

struct Ctx {
    app: AppState,
    rt: tokio::runtime::Handle,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static HWND_PTR: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Remove the icon before the process exits (otherwise a ghost icon stays until hovered).
pub fn stop() {
    let h = HWND_PTR.load(std::sync::atomic::Ordering::SeqCst);
    if !h.is_null() {
        unsafe { PostMessageW(h as HWND, WM_CLOSE, 0, 0) };
        std::thread::sleep(Duration::from_millis(150));
    }
}
static mut TASKBAR_CREATED: u32 = 0;
static mut LAST_KEY: u64 = 0;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn start(app: AppState, rt: tokio::runtime::Handle) {
    if CTX.set(Ctx { app, rt }).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(|| unsafe { run() })
        .ok();
}

/// The dashboard's branch-node mark, with a small semantic status dot.
fn make_icon(status: (u8, u8, u8)) -> HICON {
    const N: i32 = 32;
    let mut px = vec![0u8; (N * N * 4) as usize];
    let segment = |x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32| {
        let dx = bx - ax;
        let dy = by - ay;
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
    };
    for y in 0..N {
        for x in 0..N {
            let (xf, yf) = (x as f32, y as f32);
            let i = ((y * N + x) * 4) as usize;
            let inside = x >= 2
                && x < 30
                && y >= 2
                && y < 30
                && (!(x < 6 || x > 25)
                    || (!(y < 6 || y > 25))
                    || ((xf - if x < 16 { 6.0 } else { 25.0 }).powi(2)
                        + (yf - if y < 16 { 6.0 } else { 25.0 }).powi(2))
                    .sqrt()
                        <= 4.0);
            if inside {
                px[i..i + 4].copy_from_slice(&[14, 14, 16, 255]);
            }
            let node = [
                (16.0, 16.0, 3.4),
                (8.0, 8.0, 2.2),
                (24.0, 8.0, 2.2),
                (16.0, 25.0, 2.2),
            ]
            .iter()
            .any(|(a, b, r)| ((xf - a).powi(2) + (yf - b).powi(2)).sqrt() < *r);
            if node
                || segment(xf, yf, 8.0, 8.0, 16.0, 16.0) < 1.1
                || segment(xf, yf, 24.0, 8.0, 16.0, 16.0) < 1.1
                || segment(xf, yf, 16.0, 16.0, 16.0, 25.0) < 1.1
            {
                px[i..i + 4].copy_from_slice(&[237, 136, 55, 255]);
            }
            let d = ((xf - 27.0).powi(2) + (yf - 26.0).powi(2)).sqrt();
            if d < 4.8 {
                px[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
            if d < 3.2 {
                px[i..i + 4].copy_from_slice(&[status.2, status.1, status.0, 255]);
            }
        }
    }
    unsafe {
        let color = CreateBitmap(N, N, 1, 32, px.as_ptr() as _);
        let bits = vec![0u8; (N * N / 8) as usize];
        let mask = CreateBitmap(N, N, 1, 1, bits.as_ptr() as _);
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color as _);
        DeleteObject(mask as _);
        icon
    }
}

fn state_key() -> (u64, (u8, u8, u8), String) {
    let ctx = CTX.get().unwrap();
    let t = ctx.app.tunnel.status();
    let url = t.url.clone().or_else(|| {
        (!ctx.app.cfg.server.public_url.is_empty()).then(|| ctx.app.cfg.server.public_url.clone())
    });
    let (k, col) = if t.state == "online" || (t.state == "disabled" && url.is_some()) {
        (1, (46, 196, 120))
    } else if t.state.starts_with("error") {
        (3, (230, 80, 80))
    } else if t.state == "disabled" {
        (4, (110, 150, 230))
    } else {
        (2, (240, 170, 60))
    };
    let tip = format!(
        "nerve {} · {}{}",
        env!("CARGO_PKG_VERSION"),
        t.state,
        url.map(|u| format!("\n{}", u.trim_end_matches('/')))
            .unwrap_or_default()
    );
    let key = k * 1_000_003
        + tip.len() as u64
        + crate::util::sha256_hex(&tip)
            .bytes()
            .take(6)
            .fold(0u64, |a, b| a * 31 + b as u64);
    (key, col, tip)
}

unsafe fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

unsafe fn update_icon(hwnd: HWND, add: bool) {
    unsafe {
        let (key, col, tip) = state_key();
        if !add && key == LAST_KEY {
            return;
        }
        LAST_KEY = key;
        let mut n = nid(hwnd);
        n.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = make_icon(col);
        let tw: Vec<u16> = tip.encode_utf16().take(127).collect();
        n.szTip[..tw.len()].copy_from_slice(&tw);
        Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &n);
        DestroyIcon(n.hIcon);
    }
}

fn public_mcp_url() -> Option<String> {
    let ctx = CTX.get()?;
    let u = ctx.app.tunnel.url().or_else(|| {
        (!ctx.app.cfg.server.public_url.is_empty()).then(|| ctx.app.cfg.server.public_url.clone())
    })?;
    Some(format!("{}/mcp", u.trim_end_matches('/')))
}

fn open(target: &str) {
    // explorer opens URLs/folders as the shell user (non-elevated browser), never blocks us
    let mut c = std::process::Command::new("explorer.exe");
    c.arg(target);
    let _ = c.spawn();
}

fn copy_to_clipboard(s: &str) {
    use std::io::Write;
    let mut c = std::process::Command::new("clip.exe");
    crate::util::hide_window_std(&mut c);
    if let Ok(mut ch) = c.stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut i) = ch.stdin.take() {
            // clip.exe understands UTF-16LE with BOM
            let mut bytes = vec![0xFF, 0xFE];
            for u in s.encode_utf16() {
                bytes.extend_from_slice(&u.to_le_bytes());
            }
            let _ = i.write_all(&bytes);
        }
        let _ = ch.wait();
    }
}

fn color(hex: u32) -> u32 {
    ((hex >> 16) & 255) | ((hex >> 8) & 255) << 8 | (hex & 255) << 16
}
unsafe fn scale(n: i32) -> i32 {
    unsafe { (n as f32 * SCALE).round() as i32 }
}
unsafe fn text(dc: HDC, value: &str, r: RECT, size: i32, bold: bool, hex: u32) {
    unsafe {
        let name = w("Onest");
        let f = CreateFontW(
            -scale(size),
            0,
            0,
            0,
            if bold { 600 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            name.as_ptr(),
        );
        let prev = SelectObject(dc, f as _);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color(hex));
        let t = w(value);
        let mut r = r;
        DrawTextW(
            dc,
            t.as_ptr(),
            (t.len() - 1) as i32,
            &mut r,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(dc, prev);
        DeleteObject(f as _);
    }
}
unsafe fn rounded(dc: HDC, r: RECT, hex: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(color(hex));
        let prev = SelectObject(dc, brush as _);
        let pen = SelectObject(dc, GetStockObject(NULL_PEN) as _);
        RoundRect(
            dc,
            r.left,
            r.top,
            r.right,
            r.bottom,
            scale(radius),
            scale(radius),
        );
        SelectObject(dc, pen);
        SelectObject(dc, prev);
        DeleteObject(brush as _);
    }
}
unsafe fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    unsafe {
        RECT {
            left: scale(x),
            top: scale(y),
            right: scale(x + w),
            bottom: scale(y + h),
        }
    }
}
unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let light = LIGHT;
        let (bg, tx, muted, well, blue) = if light {
            (0xf5f5f5, 0x18181b, 0x5d5d68, 0xeaeaed, 0x2166c3)
        } else {
            (0x000000, 0xf4f4f5, 0xb4b4be, 0x161618, 0x3788ed)
        };
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let brush = CreateSolidBrush(color(bg));
        FillRect(dc, &client, brush);
        DeleteObject(brush as _);
        // Branch mark uses the same geometry as the dashboard logo.
        let pen = CreatePen(PS_SOLID, scale(2), color(blue));
        let oldp = SelectObject(dc, pen as _);
        let brush = CreateSolidBrush(color(blue));
        let oldb = SelectObject(dc, brush as _);
        for (ax, ay, bx, by) in [(24, 25, 34, 35), (44, 25, 34, 35), (34, 35, 34, 46)] {
            MoveToEx(dc, scale(ax), scale(ay), std::ptr::null_mut());
            LineTo(dc, scale(bx), scale(by));
        }
        for (x, y, r) in [(24, 25, 3), (44, 25, 3), (34, 35, 5), (34, 46, 3)] {
            Ellipse(dc, scale(x - r), scale(y - r), scale(x + r), scale(y + r));
        }
        SelectObject(dc, oldp);
        SelectObject(dc, oldb);
        DeleteObject(pen as _);
        DeleteObject(brush as _);
        text(dc, "nerve", rect(62, 16, 170, 36), 24, true, tx);
        text(
            dc,
            env!("CARGO_PKG_VERSION"),
            rect(254, 18, 66, 32),
            13,
            false,
            muted,
        );
        let t = CTX.get().unwrap().app.tunnel.status();
        let state = if t.state == "online" {
            "Туннель подключён"
        } else if t.state == "disabled" {
            "Локальный режим"
        } else {
            "Туннель: ожидание"
        };
        text(dc, state, rect(24, 58, 290, 24), 14, false, muted);
        for (i, (_, label)) in ITEMS.iter().enumerate() {
            let y = 96 + i as i32 * 44;
            if SELECTED == i as i32 {
                rounded(dc, rect(12, y, 316, 42), well, 12);
            }
            text(dc, label, rect(24, y, 270, 42), 15, i == 0, tx);
            text(dc, "›", rect(304, y, 16, 42), 20, false, muted);
        }
        text(
            dc,
            "Esc — закрыть",
            rect(24, 412, 240, 24),
            12,
            false,
            muted,
        );
        EndPaint(hwnd, &ps);
    }
}
unsafe fn hide_popup(hwnd: HWND) {
    unsafe {
        POPUP_OPEN = false;
        ShowWindow(hwnd, SW_HIDE);
    }
}
unsafe fn menu(hwnd: HWND) {
    unsafe {
        if POPUP_OPEN {
            hide_popup(hwnd);
            return;
        }
        LIGHT = std::fs::read_to_string(CTX.get().unwrap().app.paths.data.join("ui-theme.txt"))
            .map(|s| s.trim() == "light")
            .unwrap_or(false);
        SELECTED = 0;
        SCALE = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let (width, height) = (scale(340), scale(448));
        let work = info.rcWork;
        let x =
            pt.x.saturating_sub(width)
                .clamp(work.left, (work.right - width).max(work.left));
        let y =
            pt.y.saturating_sub(height)
                .clamp(work.top, (work.bottom - height).max(work.top));
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_SHOWWINDOW);
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, scale(24), scale(24));
        if SetWindowRgn(hwnd, region, 1) == 0 {
            DeleteObject(region as _);
        }
        POPUP_OPEN = true;
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

fn command(id: usize) {
    let ctx = CTX.get().unwrap();
    let local = format!("http://{}/ui", ctx.app.cfg.server.local_addr);
    match id {
        ID_DASH => open(&local),
        ID_COPY => copy_to_clipboard(
            &public_mcp_url()
                .unwrap_or_else(|| format!("http://{}/mcp", ctx.app.cfg.server.local_addr)),
        ),
        ID_UPDATE => {
            let app = ctx.app.clone();
            ctx.rt.spawn(async move {
                let _ = app.updates.check(&app).await;
            });
        }
        ID_TUNNEL => ctx.app.tunnel.request_restart(),
        ID_RESTART => {
            let app = ctx.app.clone();
            ctx.rt.spawn(async move {
                app.life
                    .request(Quit::Restart, true, Duration::from_millis(100))
            });
        }
        ID_FOLDER => open(&ctx.app.paths.data.to_string_lossy()),
        ID_LOGS => open(&ctx.app.paths.logs.to_string_lossy()),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                match (lp & 0xFFFF) as u32 {
                    WM_RBUTTONUP | WM_LBUTTONUP => menu(hwnd),
                    WM_LBUTTONDBLCLK => command(ID_DASH),
                    _ => {}
                }
                0
            }
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_ACTIVATE if wp & 0xFFFF == WA_INACTIVE as usize => {
                if POPUP_OPEN {
                    hide_popup(hwnd);
                }
                0
            }
            WM_KEYDOWN if POPUP_OPEN => {
                match wp as u16 {
                    VK_ESCAPE => hide_popup(hwnd),
                    VK_DOWN | VK_TAB => {
                        SELECTED = (SELECTED + 1).rem_euclid(ITEMS.len() as i32);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                    VK_UP => {
                        SELECTED = (SELECTED - 1).rem_euclid(ITEMS.len() as i32);
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                    VK_RETURN => {
                        let index = SELECTED;
                        hide_popup(hwnd);
                        if index >= 0 {
                            command(ITEMS[index as usize].0);
                        }
                    }
                    _ => {}
                };
                0
            }
            WM_MOUSEMOVE if POPUP_OPEN => {
                let y = (((lp >> 16) & 0xFFFF) as u16 as i16) as f32 / SCALE;
                let n = ((y as i32 - 96) / 44).clamp(-1, ITEMS.len() as i32);
                let n = if y < 96.0 || n >= ITEMS.len() as i32 {
                    -1
                } else {
                    n
                };
                if n != SELECTED {
                    SELECTED = n;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_LBUTTONUP if POPUP_OPEN => {
                let y = (((lp >> 16) & 0xFFFF) as u16 as i16) as f32 / SCALE;
                let index = (y as i32 - 96) / 44;
                if y >= 96.0 && index >= 0 && index < ITEMS.len() as i32 {
                    hide_popup(hwnd);
                    command(ITEMS[index as usize].0);
                }
                0
            }
            WM_TIMER => {
                update_icon(hwnd, false);
                if POPUP_OPEN {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_DPICHANGED => {
                SCALE = (wp & 0xFFFF) as f32 / 96.0;
                let r = *(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                0
            }
            WM_COMMAND => {
                command(wp & 0xFFFF);
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, 1);
                let n = nid(hwnd);
                Shell_NotifyIconW(NIM_DELETE, &n);
                PostQuitMessage(0);
                0
            }
            m if m != 0 && m == TASKBAR_CREATED => {
                // explorer restarted: re-add the icon
                update_icon(hwnd, true);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn run() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let font = include_bytes!("assets/onest-ui.ttf");
        let mut count = 0u32;
        let font_handle = AddFontMemResourceEx(
            font.as_ptr() as _,
            font.len() as u32,
            std::ptr::null_mut(),
            &mut count,
        );
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = w("nerve_tray");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        TASKBAR_CREATED = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());
        let title = w("nerve");
        // a hidden top-level window (message-only windows don't get TaskbarCreated broadcasts)
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        let hwnd = if hwnd.is_null() {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            )
        } else {
            hwnd
        };
        if hwnd.is_null() {
            tracing::warn!("tray: cannot create window");
            return;
        }
        HWND_PTR.store(hwnd as _, std::sync::atomic::Ordering::SeqCst);
        update_icon(hwnd, true);
        SetTimer(hwnd, 1, 1500, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !font_handle.is_null() {
            RemoveFontMemResourceEx(font_handle);
        }
    }
}
