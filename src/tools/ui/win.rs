//! Windows implementation of the `ui` tool: GDI capture, SendInput, window management, clipboard.
//! All coordinates are physical screen pixels (the worker thread is made per-monitor DPI aware).

use std::ffi::c_void;
use std::time::Duration;

use image::DynamicImage;
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::UiArgs;
use crate::mcp::{OrErr, ToolCtx, ToolOutput};
use crate::tools::image::{Fmt, ViewOpts, view_into};
use crate::util;

// ------------------------------------------------------------------ dispatch

pub fn dispatch(action: &str, a: UiArgs, ctx: &ToolCtx) -> Result<ToolOutput, String> {
    dpi_aware();
    if util::session_id() == Some(0) {
        return Err("nerve is running in session 0 (bare service) which has no desktop; \
install it with `nerve service install` so the server runs inside your logged-in session"
            .into());
    }
    match action {
        "screenshot" | "screen" | "capture" | "shot" => screenshot(&a, ctx),
        "ocr" | "read" | "text" => super::ocr::run(&a, ctx),
        "tree" | "elements" | "uia" | "inspect" => super::uia::tree(&a),
        "click" | "double_click" | "right_click" => click(action, &a),
        "move" | "hover" => mouse_move(&a),
        "drag" => drag(&a),
        "scroll" | "wheel" => scroll(&a),
        "type" | "write" => type_text(&a),
        "key" | "keys" | "press" | "hotkey" => keys(&a),
        "windows" | "list_windows" => Ok(ToolOutput::text(list_windows_text())),
        "window" => window_op(&a),
        "clipboard" | "clip" => clipboard(&a, ctx),
        "cursor" | "mouse" => Ok(ToolOutput::text(cursor_report())),
        other => Err(crate::tools::unknown_action(
            "ui",
            other,
            "screenshot, ocr, tree, click, move, drag, scroll, type, key, windows, window, clipboard, cursor",
        )),
    }
}

pub fn dpi_aware() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ------------------------------------------------------------------ geometry / targets

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn from_win(r: &RECT) -> Self {
        Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
    }
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
    pub fn fmt(&self) -> String {
        format!("[{},{} {}x{}]", self.x, self.y, self.w, self.h)
    }
}

pub fn virtual_screen() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

pub fn monitors() -> Vec<(Rect, bool)> {
    unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> i32 {
        let v = unsafe { &mut *(lp as *mut Vec<(Rect, bool)>) };
        let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(hm, &mut mi) } != 0 {
            v.push((Rect::from_win(&mi.rcMonitor), mi.dwFlags & 1 != 0));
        }
        1
    }
    let mut v: Vec<(Rect, bool)> = vec![];
    unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(cb), &mut v as *mut _ as LPARAM);
    }
    v.sort_by_key(|(r, p)| (!*p, r.x, r.y)); // primary first, then left-to-right
    v
}

pub enum Target {
    Rect(Rect),
    Window(HWND),
}

/// Resolve what to look at. `default_fg`: with no target, use the foreground window instead of the screen.
pub fn resolve_target(a: &UiArgs, default_fg: bool) -> Result<Target, String> {
    if let Some(r) = &a.region {
        if r.len() != 4 || r[2] <= 0 || r[3] <= 0 {
            return Err("region must be [x, y, w, h] with positive w/h (screen pixels)".into());
        }
        return Ok(Target::Rect(Rect { x: r[0], y: r[1], w: r[2], h: r[3] }));
    }
    let t = a.target.as_deref().unwrap_or("").trim().to_ascii_lowercase();
    if a.hwnd.is_some() || a.title.is_some() || t == "window" {
        return Ok(Target::Window(find_window(a.title.as_deref(), a.hwnd)?));
    }
    match t.as_str() {
        "foreground" | "active" | "focused" => Ok(Target::Window(foreground()?)),
        "" if default_fg => Ok(Target::Window(foreground()?)),
        "" | "screen" | "desktop" | "all" => Ok(Target::Rect(virtual_screen())),
        m if m.starts_with("monitor") => {
            let n: usize = m.trim_start_matches("monitor").trim_start_matches([':', ' ']).parse().unwrap_or(1);
            let ms = monitors();
            ms.get(n.saturating_sub(1)).map(|(r, _)| Target::Rect(*r)).ok_or_else(|| format!("monitor {n} not found ({} monitors)", ms.len()))
        }
        other => Err(format!("unknown target `{other}`: use screen | monitor:N | window (+title/hwnd) | foreground, or region")),
    }
}

fn foreground() -> Result<HWND, String> {
    let h = unsafe { GetForegroundWindow() };
    if h.is_null() { Err("no foreground window (locked screen or UAC prompt?)".into()) } else { Ok(h) }
}

// ------------------------------------------------------------------ windows

pub struct WinInfo {
    pub hwnd: HWND,
    pub title: String,
    pub class: String,
    pub pid: u32,
    pub rect: Rect,
    pub minimized: bool,
    pub maximized: bool,
}

fn wstr(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn window_title(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        if n <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

/// Visible frame (without the invisible resize borders) when DWM can tell us, else GetWindowRect.
pub fn window_rect(h: HWND) -> Rect {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as _, &mut r as *mut _ as *mut c_void, std::mem::size_of::<RECT>() as u32) == 0 {
            return Rect::from_win(&r);
        }
        GetWindowRect(h, &mut r);
        Rect::from_win(&r)
    }
}

fn is_cloaked(h: HWND) -> bool {
    let mut c: u32 = 0;
    const DWMWA_CLOAKED: u32 = 14;
    unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, &mut c as *mut _ as *mut c_void, 4) == 0 && c != 0 }
}

fn info(h: HWND) -> WinInfo {
    let mut cls = [0u16; 256];
    let mut pid = 0u32;
    unsafe {
        GetClassNameW(h, cls.as_mut_ptr(), cls.len() as i32);
        GetWindowThreadProcessId(h, &mut pid);
    }
    WinInfo {
        hwnd: h,
        title: window_title(h),
        class: wstr(&cls),
        pid,
        rect: window_rect(h),
        minimized: unsafe { IsIconic(h) } != 0,
        maximized: unsafe { IsZoomed(h) } != 0,
    }
}

/// User-facing top-level windows in z-order (topmost first).
pub fn list_windows() -> Vec<WinInfo> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> i32 {
        let v = unsafe { &mut *(lp as *mut Vec<HWND>) };
        v.push(h);
        1
    }
    let mut hs: Vec<HWND> = vec![];
    unsafe {
        EnumWindows(Some(cb), &mut hs as *mut _ as LPARAM);
    }
    hs.into_iter()
        .filter(|&h| unsafe {
            IsWindowVisible(h) != 0
                && GetWindowTextLengthW(h) > 0
                && (GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW) == 0
                && !is_cloaked(h)
        })
        .map(info)
        .filter(|w| w.class != "Progman" && w.class != "WorkerW")
        .collect()
}

fn list_windows_text() -> String {
    let fg = unsafe { GetForegroundWindow() };
    let ws = list_windows();
    let mut s = format!("{} windows (topmost first; * = foreground)\n", ws.len());
    for w in ws {
        let proc_ = util::process_name(w.pid).unwrap_or_default();
        let state = if w.minimized { " min" } else if w.maximized { " max" } else { "" };
        s.push_str(&format!(
            "{} hwnd {:<9} pid {:<6} {:<18} {}{}  \"{}\"\n",
            if w.hwnd == fg { '*' } else { ' ' },
            w.hwnd as isize,
            w.pid,
            proc_,
            w.rect.fmt(),
            state,
            w.title
        ));
    }
    s
}

pub fn find_window(title: Option<&str>, hwnd: Option<i64>) -> Result<HWND, String> {
    if let Some(h) = hwnd {
        let h = h as isize as HWND;
        if unsafe { IsWindow(h) } == 0 {
            return Err(format!("hwnd {} is not a window (closed?)", h as isize));
        }
        return Ok(h);
    }
    let Some(t) = title.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) else {
        return foreground();
    };
    let ws = list_windows();
    // exact title first, then substring, then process name
    if let Some(w) = ws.iter().find(|w| w.title.to_lowercase() == t) {
        return Ok(w.hwnd);
    }
    if let Some(w) = ws.iter().find(|w| w.title.to_lowercase().contains(&t)) {
        return Ok(w.hwnd);
    }
    if let Some(w) = ws.iter().find(|w| util::process_name(w.pid).map(|p| p.to_lowercase().contains(&t)).unwrap_or(false)) {
        return Ok(w.hwnd);
    }
    Err(format!("no visible window matches `{t}`; see `ui windows`"))
}

pub fn focus_window(h: HWND) -> bool {
    unsafe {
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        if GetForegroundWindow() == h {
            return true;
        }
        if SetForegroundWindow(h) != 0 && wait_foreground(h, 150) {
            return true;
        }
        // Foreground lock: become the last-input process. Alt is held around an unassigned dummy key so
        // the release is not a bare Alt tap (which would put the active window into menu mode and
        // swallow the following keys); attaching to the foreground input queue covers the rest.
        let _ = send(&[
            vk_event(VK_MENU as u16, false),
            key_input(0xE8, 0, 0),
            key_input(0xE8, 0, KEYEVENTF_KEYUP as u32),
            vk_event(VK_MENU as u16, true),
        ]);
        let fg = GetForegroundWindow();
        let cur = GetCurrentThreadId();
        let fg_t = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, std::ptr::null_mut()) };
        let attached = fg_t != 0 && fg_t != cur && AttachThreadInput(cur, fg_t, 1) != 0;
        BringWindowToTop(h);
        SetForegroundWindow(h);
        if attached {
            AttachThreadInput(cur, fg_t, 0);
        }
        wait_foreground(h, 300)
    }
}

fn wait_foreground(h: HWND, ms: u64) -> bool {
    let t = std::time::Instant::now();
    loop {
        if unsafe { GetForegroundWindow() } == h {
            std::thread::sleep(Duration::from_millis(30));
            return true;
        }
        if t.elapsed() >= Duration::from_millis(ms) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn window_op(a: &UiArgs) -> Result<ToolOutput, String> {
    let h = find_window(a.title.as_deref(), a.hwnd)?;
    let title = window_title(h);
    let op = a.op.as_deref().unwrap_or("focus").to_ascii_lowercase();
    let msg = unsafe {
        match op.as_str() {
            "focus" | "activate" | "front" => {
                if focus_window(h) { "focused".to_string() } else { "asked Windows to focus it, but it is not in front (foreground lock)".to_string() }
            }
            "minimize" | "min" => {
                ShowWindow(h, SW_MINIMIZE);
                "minimized".into()
            }
            "maximize" | "max" => {
                ShowWindow(h, SW_MAXIMIZE);
                "maximized".into()
            }
            "restore" | "normal" => {
                ShowWindow(h, SW_RESTORE);
                "restored".into()
            }
            "close" => {
                PostMessageW(h, WM_CLOSE, 0, 0);
                "close requested (the app may ask to save)".into()
            }
            "move" | "resize" => {
                let r = a.rect.as_ref().filter(|r| r.len() == 4).ok_or("move needs rect [x, y, w, h]")?;
                if IsZoomed(h) != 0 || IsIconic(h) != 0 {
                    ShowWindow(h, SW_RESTORE);
                }
                // MoveWindow sizes the outer rect, which includes invisible resize borders; compensate
                let mut wr: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut wr);
                let (outer, vis) = (Rect::from_win(&wr), window_rect(h));
                let (l, t) = (vis.x - outer.x, vis.y - outer.y);
                let (rr, b) = ((outer.x + outer.w) - (vis.x + vis.w), (outer.y + outer.h) - (vis.y + vis.h));
                MoveWindow(h, r[0] - l, r[1] - t, r[2] + l + rr, r[3] + t + b, 1);
                format!("moved to {}", window_rect(h).fmt())
            }
            o => return Err(format!("unknown window op `{o}`: focus | minimize | maximize | restore | close | move")),
        }
    };
    Ok(ToolOutput::text(format!("\"{title}\" (hwnd {}): {msg}", h as isize)))
}

// ------------------------------------------------------------------ capture

pub struct Shot {
    pub rect: Rect,
    pub bgra: Vec<u8>,
}

impl Shot {
    pub fn to_image(&self) -> DynamicImage {
        let mut rgb = Vec::with_capacity(self.bgra.len() / 4 * 3);
        for p in self.bgra.chunks_exact(4) {
            rgb.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        DynamicImage::ImageRgb8(image::RgbImage::from_raw(self.rect.w as u32, self.rect.h as u32, rgb).expect("buffer size"))
    }
    fn is_blank(&self) -> bool {
        self.bgra.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
    }
}

unsafe fn read_bitmap(dc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Result<Vec<u8>, String> {
    let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // top-down
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB as _;
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let n = unsafe { GetDIBits(dc, bmp, 0, h as u32, buf.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS) };
    if n == 0 {
        return Err("GetDIBits failed".into());
    }
    Ok(buf)
}

/// Copy a rectangle of the visible screen (all monitors, layered windows included).
pub fn capture_rect(r: Rect) -> Result<Shot, String> {
    let vs = virtual_screen();
    let r = r.intersect(&vs).ok_or_else(|| format!("region {} is outside the screen {}", r.fmt(), vs.fmt()))?;
    unsafe {
        let sdc = GetDC(std::ptr::null_mut());
        if sdc.is_null() {
            return Err("cannot open the screen (locked workstation or secure desktop?)".into());
        }
        let mdc = CreateCompatibleDC(sdc);
        let bmp = CreateCompatibleBitmap(sdc, r.w, r.h);
        let old = SelectObject(mdc, bmp as HGDIOBJ);
        let ok = BitBlt(mdc, 0, 0, r.w, r.h, sdc, r.x, r.y, SRCCOPY | CAPTUREBLT) != 0;
        SelectObject(mdc, old);
        let px = if ok { read_bitmap(mdc, bmp, r.w, r.h) } else { Err("BitBlt failed (locked workstation or UAC prompt?)".into()) };
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mdc);
        ReleaseDC(std::ptr::null_mut(), sdc);
        Ok(Shot { rect: r, bgra: px? })
    }
}

/// Capture one window even when covered by others (PrintWindow full-content render); falls back to the
/// screen pixels when the app doesn't support it (some GPU surfaces render black).
pub fn capture_window(h: HWND) -> Result<Shot, String> {
    unsafe {
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
            std::thread::sleep(Duration::from_millis(250));
        }
        let mut wr: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut wr);
        let full = Rect::from_win(&wr);
        let vis = window_rect(h);
        if full.w <= 0 || full.h <= 0 {
            return Err("window has no size".into());
        }
        let sdc = GetDC(std::ptr::null_mut());
        let mdc = CreateCompatibleDC(sdc);
        let bmp = CreateCompatibleBitmap(sdc, full.w, full.h);
        let old = SelectObject(mdc, bmp as HGDIOBJ);
        const PW_RENDERFULLCONTENT: u32 = 2;
        let ok = PrintWindow(h, mdc, PW_RENDERFULLCONTENT) != 0;
        SelectObject(mdc, old);
        let px = if ok { read_bitmap(mdc, bmp, full.w, full.h).ok() } else { None };
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mdc);
        ReleaseDC(std::ptr::null_mut(), sdc);
        if let Some(px) = px {
            // crop the invisible resize borders away
            let crop = vis.intersect(&full).unwrap_or(full);
            let (ox, oy) = ((crop.x - full.x) as usize, (crop.y - full.y) as usize);
            let mut out = Vec::with_capacity(crop.w as usize * crop.h as usize * 4);
            for row in 0..crop.h as usize {
                let s = ((oy + row) * full.w as usize + ox) * 4;
                out.extend_from_slice(&px[s..s + crop.w as usize * 4]);
            }
            let shot = Shot { rect: crop, bgra: out };
            if !shot.is_blank() {
                return Ok(shot);
            }
        }
        // fallback: bring it forward and grab the screen pixels
        focus_window(h);
        std::thread::sleep(Duration::from_millis(150));
        capture_rect(window_rect(h))
    }
}

pub fn capture_target(t: &Target) -> Result<(Shot, String), String> {
    match t {
        Target::Rect(r) => Ok((capture_rect(*r)?, "screen".into())),
        Target::Window(h) => Ok((capture_window(*h)?, format!("window \"{}\"", window_title(*h)))),
    }
}

fn save_png(ctx: &ToolCtx, img: &DynamicImage) -> Option<std::path::PathBuf> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let dir = ctx.app.paths.out.join("screens");
    std::fs::create_dir_all(&dir).ok()?;
    // keep the 30 newest
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut fs: Vec<_> = rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "png")).collect();
        if fs.len() >= 30 {
            fs.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            for e in &fs[..fs.len() - 29] {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let p = dir.join(format!("shot-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")));
    let f = std::fs::File::create(&p).ok()?;
    img.write_with_encoder(PngEncoder::new_with_quality(std::io::BufWriter::new(f), CompressionType::Fast, FilterType::Sub)).ok()?;
    Some(p)
}

pub fn view_opts(a: &UiArgs, ctx: &ToolCtx, origin: (i32, i32)) -> Result<ViewOpts, String> {
    let cfg = &ctx.app.cfg.image;
    let fmt = match a.format.as_deref().unwrap_or("auto") {
        "auto" | "" => Fmt::Auto,
        "png" => Fmt::Png,
        "jpeg" | "jpg" => Fmt::Jpeg,
        o => return Err(format!("unknown format `{o}` (auto|png|jpeg)")),
    };
    Ok(ViewOpts {
        region: None,
        tile: a.tile.clone(),
        scale: a.scale,
        budget: a.max_bytes.unwrap_or(cfg.max_bytes).max(4096),
        max_dim: a.max_dim.unwrap_or(cfg.max_dim),
        fmt,
        min_q: cfg.min_quality,
        origin,
        coord: "screen",
    })
}

fn screenshot(a: &UiArgs, ctx: &ToolCtx) -> Result<ToolOutput, String> {
    let t = resolve_target(a, false)?;
    let (shot, what) = capture_target(&t)?;
    let img = shot.to_image();
    let saved = save_png(ctx, &img);
    let mut out = ToolOutput::empty();
    let o = view_opts(a, ctx, (shot.rect.x, shot.rect.y))?;
    view_into(&img, &format!("{what} {}", shot.rect.fmt()), &o, &mut out)?;
    if let Some(p) = saved {
        out.push_text(format!("full-res PNG: {} (zoom: image {{path, region, scale}})", p.display()));
    }
    Ok(out)
}

// ------------------------------------------------------------------ input primitives

fn send(inputs: &[INPUT]) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        return Err("input was blocked (UAC prompt / locked screen, or the target runs at a higher integrity level)".into());
    }
    Ok(())
}

fn mouse_input(flags: u32, data: i32) -> INPUT {
    let mut i: INPUT = unsafe { std::mem::zeroed() };
    i.r#type = INPUT_MOUSE;
    i.Anonymous.mi = MOUSEINPUT { dx: 0, dy: 0, mouseData: data as _, dwFlags: flags as _, time: 0, dwExtraInfo: 0 };
    i
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut i: INPUT = unsafe { std::mem::zeroed() };
    i.r#type = INPUT_KEYBOARD;
    i.Anonymous.ki = KEYBDINPUT { wVk: vk as _, wScan: scan, dwFlags: flags as _, time: 0, dwExtraInfo: 0 };
    i
}

fn is_extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2C | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0x6F | 0x90 | 0xA3 | 0xA5)
}

fn vk_event(vk: u16, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut f = 0u32;
    if up {
        f |= KEYEVENTF_KEYUP as u32;
    }
    if is_extended(vk) {
        f |= KEYEVENTF_EXTENDEDKEY as u32;
    }
    key_input(vk, scan, f)
}

fn set_cursor(x: i32, y: i32) -> Result<(), String> {
    if unsafe { SetCursorPos(x, y) } == 0 {
        return Err(format!("SetCursorPos({x},{y}) failed (secure desktop?)"));
    }
    Ok(())
}

fn cursor() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

fn window_at(x: i32, y: i32) -> String {
    unsafe {
        let h = WindowFromPoint(POINT { x, y });
        if h.is_null() {
            return String::new();
        }
        let root = GetAncestor(h, GA_ROOT);
        let t = window_title(if root.is_null() { h } else { root });
        if t.is_empty() { String::new() } else { format!(" over \"{t}\"") }
    }
}

fn cursor_report() -> String {
    let (x, y) = cursor();
    format!("cursor at ({x},{y}){}", window_at(x, y))
}

fn button_flags(b: Option<&str>) -> Result<(u32, u32, &'static str), String> {
    Ok(match b.unwrap_or("left").to_ascii_lowercase().as_str() {
        "left" | "l" | "" => (MOUSEEVENTF_LEFTDOWN as u32, MOUSEEVENTF_LEFTUP as u32, "left"),
        "right" | "r" => (MOUSEEVENTF_RIGHTDOWN as u32, MOUSEEVENTF_RIGHTUP as u32, "right"),
        "middle" | "m" | "wheel" => (MOUSEEVENTF_MIDDLEDOWN as u32, MOUSEEVENTF_MIDDLEUP as u32, "middle"),
        o => return Err(format!("unknown button `{o}` (left|right|middle)")),
    })
}

// ------------------------------------------------------------------ mouse actions

/// Where to click: explicit x/y, or an element/text found via UI Automation, then OCR.
fn click_point(a: &UiArgs) -> Result<((i32, i32), String), String> {
    if let (Some(x), Some(y)) = (a.x, a.y) {
        return Ok(((x, y), String::new()));
    }
    let Some(q) = a.text.as_deref().or(a.filter.as_deref()).map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("click needs x and y, or `text` (an element name / visible text to click)".into());
    };
    if let Some((r, label)) = super::uia::find_element(a, q)? {
        return Ok((r.center(), format!(" on element {label} {}", r.fmt())));
    }
    if let Some((r, line)) = super::ocr::find_text(a, q)? {
        return Ok((r.center(), format!(" on text \"{line}\" {} (OCR)", r.fmt())));
    }
    Err(format!("nothing named or showing `{q}` was found (try ui tree / ui ocr / ui screenshot)"))
}

fn click(action: &str, a: &UiArgs) -> Result<ToolOutput, String> {
    let ((x, y), how) = click_point(a)?;
    let btn = if action == "right_click" { Some("right") } else { a.button.as_deref() };
    let (down, up, name) = button_flags(btn)?;
    let count = if action == "double_click" { 2 } else { a.count.unwrap_or(1).clamp(1, 3) };
    set_cursor(x, y)?;
    std::thread::sleep(Duration::from_millis(20));
    for i in 0..count {
        send(&[mouse_input(down, 0), mouse_input(up, 0)])?;
        if i + 1 < count {
            std::thread::sleep(Duration::from_millis(45));
        }
    }
    let what = match count {
        2 => "double-clicked",
        3 => "triple-clicked",
        _ => "clicked",
    };
    Ok(ToolOutput::text(format!("{name} {what} at ({x},{y}){how}{}", window_at(x, y))))
}

fn mouse_move(a: &UiArgs) -> Result<ToolOutput, String> {
    let ((x, y), how) = click_point(a)?;
    set_cursor(x, y)?;
    // a zero relative move makes apps see a real WM_MOUSEMOVE (hover effects, tooltips)
    send(&[mouse_input(MOUSEEVENTF_MOVE as u32, 0)])?;
    Ok(ToolOutput::text(format!("moved to ({x},{y}){how}{}", window_at(x, y))))
}

fn drag(a: &UiArgs) -> Result<ToolOutput, String> {
    let (Some(x), Some(y), Some(tx), Some(ty)) = (a.x, a.y, a.to_x, a.to_y) else {
        return Err("drag needs x, y, to_x, to_y".into());
    };
    let (down, up, name) = button_flags(a.button.as_deref())?;
    set_cursor(x, y)?;
    std::thread::sleep(Duration::from_millis(30));
    send(&[mouse_input(down, 0)])?;
    std::thread::sleep(Duration::from_millis(60));
    let steps = (((tx - x).abs().max((ty - y).abs())) / 12).clamp(8, 60);
    for i in 1..=steps {
        let px = x + (tx - x) * i / steps;
        let py = y + (ty - y) * i / steps;
        set_cursor(px, py)?;
        let _ = send(&[mouse_input(MOUSEEVENTF_MOVE as u32, 0)]);
        std::thread::sleep(Duration::from_millis(8));
    }
    std::thread::sleep(Duration::from_millis(60));
    send(&[mouse_input(up, 0)])?;
    Ok(ToolOutput::text(format!("{name}-dragged ({x},{y}) -> ({tx},{ty}){}", window_at(tx, ty))))
}

fn scroll(a: &UiArgs) -> Result<ToolOutput, String> {
    if let (Some(x), Some(y)) = (a.x, a.y) {
        set_cursor(x, y)?;
        std::thread::sleep(Duration::from_millis(20));
    }
    let (dy, dx) = (a.dy.unwrap_or(0), a.dx.unwrap_or(0));
    if dy == 0 && dx == 0 {
        return Err("scroll needs dy (+up/-down notches) and/or dx (+right/-left)".into());
    }
    let mut v = vec![];
    if dy != 0 {
        v.push(mouse_input(MOUSEEVENTF_WHEEL as u32, dy * 120));
    }
    if dx != 0 {
        v.push(mouse_input(MOUSEEVENTF_HWHEEL as u32, dx * 120));
    }
    send(&v)?;
    let (x, y) = cursor();
    Ok(ToolOutput::text(format!("scrolled dy={dy} dx={dx} at ({x},{y}){}", window_at(x, y))))
}

// ------------------------------------------------------------------ keyboard

fn focus_from_args(a: &UiArgs) -> Result<(), String> {
    if let Some(t) = a.title.as_deref().filter(|t| !t.is_empty()) {
        focus_window(find_window(Some(t), None)?);
    } else if let Some(h) = a.hwnd {
        focus_window(find_window(None, Some(h))?);
    }
    Ok(())
}

/// One character as key events. Uses the real key of the target window's keyboard layout when there
/// is one (apps like the new Notepad garble bursts of VK_PACKET input), else a Unicode packet.
fn char_inputs(c: char, layout: HKL) -> Vec<INPUT> {
    let tap = |vk: u16| vec![vk_event(vk, false), vk_event(vk, true)];
    match c {
        '\n' => return tap(VK_RETURN as u16),
        '\t' => return tap(VK_TAB as u16),
        _ => {}
    }
    if (c as u32) < 0x10000 {
        let r = unsafe { VkKeyScanExW(c as u16, layout) };
        if r != -1 {
            let vk = (r as u16) & 0xFF;
            let mods = (r as u16 >> 8) & 0xFF;
            // only plain or shifted keys; Ctrl/Alt combos (AltGr) go through Unicode instead
            if mods & !1 == 0 && vk != 0 {
                let mut v = vec![];
                if mods & 1 != 0 {
                    v.push(vk_event(VK_SHIFT as u16, false));
                }
                v.push(vk_event(vk, false));
                v.push(vk_event(vk, true));
                if mods & 1 != 0 {
                    v.push(vk_event(VK_SHIFT as u16, true));
                }
                return v;
            }
        }
    }
    let mut v = vec![];
    let mut b = [0u16; 2];
    for u in c.encode_utf16(&mut b) {
        v.push(key_input(0, *u, KEYEVENTF_UNICODE as u32));
        v.push(key_input(0, *u, KEYEVENTF_UNICODE as u32 | KEYEVENTF_KEYUP as u32));
    }
    v
}

fn modifiers_down() -> bool {
    [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN].iter().any(|&k| unsafe { GetAsyncKeyState(k as i32) } as u16 & 0x8000 != 0)
}

fn type_keys(text: &str) -> Result<(), String> {
    // a physically held modifier would turn letters into shortcuts
    for _ in 0..20 {
        if !modifiers_down() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let fg = unsafe { GetForegroundWindow() };
    let layout = unsafe { GetKeyboardLayout(GetWindowThreadProcessId(fg, std::ptr::null_mut())) };
    for c in text.chars().filter(|c| *c != '\r') {
        send(&char_inputs(c, layout))?;
        std::thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}

fn type_paste(text: &str) -> Result<(), String> {
    let saved = clip_get_text().ok().flatten();
    clip_set_text(&text.replace("\r\n", "\n").replace('\n', "\r\n"))?;
    std::thread::sleep(Duration::from_millis(40));
    send(&[vk_event(VK_CONTROL as u16, false), vk_event(b'V' as u16, false), vk_event(b'V' as u16, true), vk_event(VK_CONTROL as u16, true)])?;
    // give the app time to read the clipboard before restoring it
    std::thread::sleep(Duration::from_millis(250));
    if let Some(old) = saved {
        let _ = clip_set_text(&old);
    }
    Ok(())
}

fn type_text(a: &UiArgs) -> Result<ToolOutput, String> {
    let text = a.text.as_deref().or(a.value.as_deref()).ok_or("type needs `text`")?;
    focus_from_args(a)?;
    let n = text.chars().filter(|c| *c != '\r').count();
    let mode = match a.mode.as_deref().unwrap_or("auto").to_ascii_lowercase().as_str() {
        "auto" | "" => {
            if n > 300 { "paste" } else { "keys" }
        }
        "keys" | "key" | "keyboard" => "keys",
        "paste" | "clipboard" => "paste",
        o => return Err(format!("unknown type mode `{o}` (auto|keys|paste)")),
    };
    if mode == "paste" { type_paste(text)? } else { type_keys(text)? }
    let fg = unsafe { GetForegroundWindow() };
    Ok(ToolOutput::text(format!("typed {n} chars ({mode}) into \"{}\"", window_title(fg))))
}

fn key_vk(name: &str) -> Result<(u16, bool), String> {
    // (vk, needs_shift)
    let n = name.trim().to_ascii_lowercase();
    let vk: u16 = match n.as_str() {
        "ctrl" | "control" | "ctl" => 0x11,
        "shift" => 0x10,
        "alt" | "menu" | "option" => 0x12,
        "win" | "windows" | "meta" | "cmd" | "super" | "lwin" => 0x5B,
        "rwin" => 0x5C,
        "rctrl" => 0xA3,
        "ralt" | "altgr" => 0xA5,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "tab" => 0x09,
        "space" | "spacebar" => 0x20,
        "backspace" | "bksp" | "back" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "capslock" | "caps" => 0x14,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        "printscreen" | "prtsc" | "print" => 0x2C,
        "pause" | "break" => 0x13,
        "apps" | "contextmenu" => 0x5D,
        "volumeup" | "volume_up" => 0xAF,
        "volumedown" | "volume_down" => 0xAE,
        "mute" | "volume_mute" => 0xAD,
        "playpause" | "play_pause" | "play" => 0xB3,
        "next" | "nexttrack" => 0xB0,
        "prev" | "prevtrack" => 0xB1,
        "stop" => 0xB2,
        "plus" => 0xBB,
        "minus" => 0xBD,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            let k: u16 = f[1..].parse().map_err(|_| format!("bad key `{name}`"))?;
            if !(1..=24).contains(&k) {
                return Err(format!("bad key `{name}`"));
            }
            0x70 + k - 1
        }
        s if s.chars().count() == 1 => {
            let c = name.trim().chars().next().unwrap();
            if c.is_ascii_alphabetic() {
                return Ok((c.to_ascii_uppercase() as u16, false));
            }
            if c.is_ascii_digit() {
                return Ok((c as u16, false));
            }
            let r = unsafe { VkKeyScanW(c as u16) };
            if r == -1 {
                return Err(format!("key `{c}` is not on the current keyboard layout; use `type` for text"));
            }
            return Ok(((r as u16) & 0xFF, (r as u16 >> 8) & 1 != 0));
        }
        _ => return Err(format!("unknown key `{name}`")),
    };
    Ok((vk, false))
}

fn keys(a: &UiArgs) -> Result<ToolOutput, String> {
    let spec = a.keys.as_deref().or(a.text.as_deref()).ok_or("key needs `keys`, e.g. \"ctrl+s\" or \"alt+tab\" or \"tab*3 enter\"")?;
    focus_from_args(a)?;
    let mut done = vec![];
    for chord in spec.split([' ', ',']).map(str::trim).filter(|s| !s.is_empty()) {
        let (chord, times) = match chord.rsplit_once('*') {
            Some((c, n)) if n.chars().all(|d| d.is_ascii_digit()) && !n.is_empty() => (c, n.parse::<u32>().unwrap_or(1).clamp(1, 100)),
            _ => (chord, 1),
        };
        // "ctrl++" means ctrl and the plus key
        let parts: Vec<String> = if chord.ends_with("++") {
            let mut p: Vec<String> = chord[..chord.len() - 2].split('+').map(String::from).collect();
            p.push("plus".into());
            p
        } else {
            chord.split('+').map(String::from).collect()
        };
        let mut vks = vec![];
        for p in &parts {
            let (vk, shift) = key_vk(p)?;
            if shift && !vks.contains(&0x10) {
                vks.push(0x10);
            }
            vks.push(vk);
        }
        for _ in 0..times {
            let mut seq: Vec<INPUT> = vks.iter().map(|&k| vk_event(k, false)).collect();
            seq.extend(vks.iter().rev().map(|&k| vk_event(k, true)));
            send(&seq)?;
            std::thread::sleep(Duration::from_millis(25));
        }
        done.push(if times > 1 { format!("{chord}*{times}") } else { chord.to_string() });
    }
    let fg = unsafe { GetForegroundWindow() };
    Ok(ToolOutput::text(format!("pressed {} in \"{}\"", done.join(" "), window_title(fg))))
}

// ------------------------------------------------------------------ clipboard

struct Clip;

impl Clip {
    fn open() -> Result<Clip, String> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Clip);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err("clipboard is busy (another app holds it open)".into())
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

const CF_UNICODETEXT_: u32 = 13;
const CF_DIB_: u32 = 8;

fn clip_get_text() -> Result<Option<String>, String> {
    let _c = Clip::open()?;
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT_) == 0 {
            return Ok(None);
        }
        let h = GetClipboardData(CF_UNICODETEXT_);
        if h.is_null() {
            return Ok(None);
        }
        let p = GlobalLock(h as _) as *const u16;
        if p.is_null() {
            return Ok(None);
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        GlobalUnlock(h as _);
        Ok(Some(s))
    }
}

fn clip_get_image() -> Result<Option<DynamicImage>, String> {
    let _c = Clip::open()?;
    unsafe {
        if IsClipboardFormatAvailable(CF_DIB_) == 0 {
            return Ok(None);
        }
        let h = GetClipboardData(CF_DIB_);
        if h.is_null() {
            return Ok(None);
        }
        let size = GlobalSize(h as _);
        let p = GlobalLock(h as _) as *const u8;
        if p.is_null() || size < std::mem::size_of::<BITMAPINFOHEADER>() {
            return Ok(None);
        }
        let data = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h as _);
        let hdr = &*(data.as_ptr() as *const BITMAPINFOHEADER);
        let (w, hgt, bpp) = (hdr.biWidth, hdr.biHeight, hdr.biBitCount);
        if w <= 0 || hgt == 0 || !(bpp == 24 || bpp == 32) {
            return Err(format!("clipboard image format not supported ({bpp} bpp)"));
        }
        let top_down = hgt < 0;
        let hh = hgt.unsigned_abs() as usize;
        let w = w as usize;
        let mut off = hdr.biSize as usize;
        if hdr.biCompression == 3 && hdr.biSize == 40 {
            off += 12; // BI_BITFIELDS masks
        }
        let stride = (w * bpp as usize / 8 + 3) & !3;
        if data.len() < off + stride * hh {
            return Err("clipboard image is truncated".into());
        }
        let mut rgb = Vec::with_capacity(w * hh * 3);
        for row in 0..hh {
            let r = if top_down { row } else { hh - 1 - row };
            let line = &data[off + r * stride..];
            for x in 0..w {
                let i = x * bpp as usize / 8;
                rgb.extend_from_slice(&[line[i + 2], line[i + 1], line[i]]);
            }
        }
        Ok(image::RgbImage::from_raw(w as u32, hh as u32, rgb).map(DynamicImage::ImageRgb8))
    }
}

fn clip_set_text(s: &str) -> Result<(), String> {
    let _c = Clip::open()?;
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if h.is_null() {
            return Err("GlobalAlloc failed".into());
        }
        let p = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT_, h as _).is_null() {
            GlobalFree(h);
            return Err("SetClipboardData failed".into());
        }
    }
    Ok(())
}

fn clipboard(a: &UiArgs, ctx: &ToolCtx) -> Result<ToolOutput, String> {
    let set = a.set.as_deref().or(a.value.as_deref().filter(|v| *v != "get")).or(a.text.as_deref());
    if let Some(s) = set {
        clip_set_text(s)?;
        return Ok(ToolOutput::text(format!("clipboard set ({} chars)", s.chars().count())));
    }
    if let Some(t) = clip_get_text()? {
        let n = t.chars().count();
        let max = ctx.app.cfg.exec.inline_output_bytes.max(1000);
        return Ok(ToolOutput::text(format!("clipboard text ({n} chars):\n{}", util::clip_middle(&t, max))));
    }
    if let Some(img) = clip_get_image()? {
        let mut out = ToolOutput::empty();
        let o = ViewOpts { coord: "original", origin: (0, 0), ..view_opts(a, ctx, (0, 0))? };
        view_into(&img, "clipboard image", &o, &mut out)?;
        return Ok(out);
    }
    Ok(ToolOutput::text("clipboard is empty (or holds a format other than text/image)"))
}

#[allow(dead_code)]
pub fn ctx_err<T>(r: Result<T, impl std::fmt::Display>, what: &str) -> Result<T, String> {
    r.ctx(what)
}
