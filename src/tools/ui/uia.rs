//! UI Automation: the element tree of a window (roles, names, values, screen rects) and element search.
//! Uses one cache request per walk so every property comes back in a single cross-process round trip.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Accessibility::*;

use super::UiArgs;
use super::win::{Rect, Target, resolve_target, window_title};
use crate::mcp::{OrErr, ToolOutput};

pub struct El {
    pub depth: u32,
    pub role: &'static str,
    pub name: String,
    pub value: Option<String>,
    pub aid: String,
    pub rect: Rect,
    pub enabled: bool,
    pub focused: bool,
    pub interactive: bool,
}

fn role(id: i32) -> (&'static str, bool) {
    match id {
        50000 => ("Button", true),
        50001 => ("Calendar", true),
        50002 => ("CheckBox", true),
        50003 => ("ComboBox", true),
        50004 => ("Edit", true),
        50005 => ("Link", true),
        50006 => ("Image", false),
        50007 => ("ListItem", true),
        50008 => ("List", false),
        50009 => ("Menu", false),
        50010 => ("MenuBar", false),
        50011 => ("MenuItem", true),
        50012 => ("ProgressBar", false),
        50013 => ("Radio", true),
        50014 => ("ScrollBar", false),
        50015 => ("Slider", true),
        50016 => ("Spinner", true),
        50017 => ("StatusBar", false),
        50018 => ("Tab", false),
        50019 => ("TabItem", true),
        50020 => ("Text", false),
        50021 => ("ToolBar", false),
        50022 => ("ToolTip", false),
        50023 => ("Tree", false),
        50024 => ("TreeItem", true),
        50025 => ("Custom", false),
        50026 => ("Group", false),
        50027 => ("Thumb", false),
        50028 => ("DataGrid", false),
        50029 => ("DataItem", true),
        50030 => ("Document", true),
        50031 => ("SplitButton", true),
        50032 => ("Window", false),
        50033 => ("Pane", false),
        50034 => ("Header", false),
        50035 => ("HeaderItem", true),
        50036 => ("Table", false),
        50037 => ("TitleBar", false),
        50038 => ("Separator", false),
        50039 => ("SemanticZoom", false),
        50040 => ("AppBar", false),
        _ => ("Element", false),
    }
}

pub fn automation() -> Result<IUIAutomation, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ctx("UI Automation")
    }
}

fn cache(ua: &IUIAutomation) -> Result<IUIAutomationCacheRequest, String> {
    unsafe {
        let c = ua.CreateCacheRequest().ctx("cache request")?;
        for p in [
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_BoundingRectanglePropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_IsEnabledPropertyId,
            UIA_AutomationIdPropertyId,
            UIA_HasKeyboardFocusPropertyId,
        ] {
            c.AddProperty(p).ctx("cache property")?;
        }
        Ok(c)
    }
}

struct Walk<'a> {
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
    max_depth: u32,
    limit: usize,
    deadline: Instant,
    clip: Option<Rect>,
    out: &'a mut Vec<El>,
    truncated: bool,
}

impl Walk<'_> {
    fn visit(&mut self, e: &IUIAutomationElement, depth: u32) {
        if self.out.len() >= self.limit * 4 || Instant::now() > self.deadline {
            self.truncated = true;
            return;
        }
        unsafe {
            let (role, inter) = role(e.CachedControlType().map(|c| c.0).unwrap_or(0));
            let r = e.CachedBoundingRectangle().map(|r| Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
            let rect = r.unwrap_or(Rect { x: 0, y: 0, w: 0, h: 0 });
            let off = e.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false);
            // whole subtree outside the clip region: skip it
            if let Some(c) = &self.clip {
                if rect.w > 0 && rect.h > 0 && rect.intersect(c).is_none() && depth > 0 {
                    return;
                }
            }
            self.out.push(El {
                depth,
                role,
                name: e.CachedName().map(|s| s.to_string()).unwrap_or_default(),
                value: None,
                aid: e.CachedAutomationId().map(|s| s.to_string()).unwrap_or_default(),
                rect,
                enabled: e.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true),
                focused: e.CachedHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false),
                interactive: inter && !off && rect.w > 0 && rect.h > 0,
            });
            if role == "Edit" || role == "ComboBox" || role == "Document" {
                if let Ok(vp) = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                    if let Ok(v) = vp.CurrentValue() {
                        let v = v.to_string();
                        if !v.is_empty() {
                            self.out.last_mut().unwrap().value = Some(v);
                        }
                    }
                }
            }
            if depth >= self.max_depth {
                return;
            }
            let mut child = self.walker.GetFirstChildElementBuildCache(e, &self.cache).ok();
            while let Some(c) = child {
                self.visit(&c, depth + 1);
                if self.truncated {
                    return;
                }
                child = self.walker.GetNextSiblingElementBuildCache(&c, &self.cache).ok();
            }
        }
    }
}

/// Walk the target (default: foreground window). Returns (elements, description, truncated).
pub fn collect(a: &UiArgs, max_depth: u32) -> Result<(Vec<El>, String, bool), String> {
    let ua = automation()?;
    let c = cache(&ua)?;
    let (root, what, clip) = unsafe {
        match resolve_target(a, true)? {
            Target::Window(h) => (
                ua.ElementFromHandleBuildCache(HWND(h), &c).ctx("element from window")?,
                format!("window \"{}\" (hwnd {})", window_title(h), h as isize),
                None,
            ),
            Target::Rect(r) => (ua.GetRootElementBuildCache(&c).ctx("desktop root")?, format!("screen region {}", r.fmt()), Some(r)),
        }
    };
    let mut out = vec![];
    let mut w = Walk {
        walker: unsafe { ua.ControlViewWalker().ctx("tree walker")? },
        cache: c,
        max_depth,
        limit: a.limit.unwrap_or(400).clamp(10, 5000),
        deadline: Instant::now() + Duration::from_secs(12),
        clip,
        out: &mut out,
        truncated: false,
    };
    w.visit(&root, 0);
    let truncated = w.truncated;
    Ok((out, what, truncated))
}

fn clip(s: &str, n: usize) -> String {
    let s = s.replace(['\r', '\n'], " ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

pub fn tree(a: &UiArgs) -> Result<ToolOutput, String> {
    let t0 = Instant::now();
    let depth = a.depth.unwrap_or(12).clamp(1, 40);
    let (els, what, truncated) = collect(a, depth)?;
    let only = a.interactive_only.unwrap_or(true);
    let filt = a.filter.as_deref().map(str::to_lowercase).filter(|f| !f.is_empty());
    let limit = a.limit.unwrap_or(400).clamp(10, 5000);
    let mut s = String::new();
    let mut shown = 0;
    for e in &els {
        let has_text = !e.name.trim().is_empty() || e.value.is_some();
        if only && !(e.interactive || (has_text && e.role == "Text" && e.rect.w > 0)) {
            continue;
        }
        if !only && e.depth > 0 && !has_text && !e.interactive && e.aid.is_empty() {
            continue; // nameless layout containers add noise
        }
        if let Some(f) = &filt {
            let hay = format!("{} {} {}", e.name, e.value.as_deref().unwrap_or(""), e.aid).to_lowercase();
            if !hay.contains(f) {
                continue;
            }
        }
        if shown >= limit {
            s.push_str(&format!("… limit {limit} reached; narrow with filter/region or raise limit\n"));
            break;
        }
        shown += 1;
        let indent = if only { String::new() } else { "  ".repeat(e.depth.min(20) as usize) };
        let (cx, cy) = e.rect.center();
        let mut line = format!("{indent}({cx},{cy}) {} \"{}\"", e.role, clip(&e.name, 80));
        if let Some(v) = &e.value {
            line.push_str(&format!(" value=\"{}\"", clip(v, 120)));
        }
        if !e.aid.is_empty() && e.aid.len() <= 40 && !e.aid.chars().all(|c| c.is_ascii_digit()) {
            line.push_str(&format!(" id={}", e.aid));
        }
        line.push_str(&format!(" {}", e.rect.fmt()));
        if !e.enabled {
            line.push_str(" disabled");
        }
        if e.focused {
            line.push_str(" FOCUSED");
        }
        s.push_str(&line);
        s.push('\n');
    }
    let head = format!(
        "UI tree of {what} · {shown} shown / {} walked · {}ms{}\n(center) Role \"name\" [x,y wxh] — click the center\n",
        els.len(),
        t0.elapsed().as_millis(),
        if truncated { " · TRUNCATED (time/size cap)" } else { "" }
    );
    let mut body = head + &s;
    if shown == 0 {
        body.push_str("(no accessible elements — games, canvas or custom-drawn apps expose none; use ui ocr or ui screenshot)\n");
    }
    Ok(ToolOutput::text(body))
}

/// Find the best element whose name/automation id matches `q`.
pub fn find_element(a: &UiArgs, q: &str) -> Result<Option<(Rect, String)>, String> {
    let (els, _, _) = match collect(a, 30) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let ql = q.trim().to_lowercase();
    let score = |e: &El| -> Option<u32> {
        if e.rect.w <= 0 || e.rect.h <= 0 {
            return None;
        }
        let n = e.name.trim().to_lowercase();
        let base = if n == ql || e.aid.to_lowercase() == ql {
            0
        } else if n.starts_with(&ql) {
            10
        } else if n.contains(&ql) {
            20
        } else {
            return None;
        };
        Some(base + if e.interactive { 0 } else { 5 } + if e.enabled { 0 } else { 3 })
    };
    let best = els.iter().filter_map(|e| score(e).map(|s| (s, e))).min_by_key(|(s, e)| (*s, e.name.len()));
    Ok(best.map(|(_, e)| (e.rect, format!("{} \"{}\"", e.role, clip(&e.name, 60)))))
}
