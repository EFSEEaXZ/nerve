//! `ui`: the desktop. Screenshots, OCR, the UI-element tree, mouse, keyboard, windows and the clipboard.
//! Windows is fully supported; other OSes return a clear "not supported on this platform" error per action
//! so the tool shape stays identical everywhere.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::mcp::{ToolCtx, ToolDef, ToolOutput, blocking, tool};

#[cfg(windows)]
mod ocr;
#[cfg(windows)]
mod uia;
#[cfg(windows)]
mod win;

#[derive(Deserialize, JsonSchema)]
pub struct UiArgs {
    /// screenshot | ocr | tree | click | move | drag | scroll | type | key | windows | window | clipboard
    action: String,

    // ---- targeting (screenshot / ocr / tree) ----
    /// Capture/scan target: "screen" (default, virtual desktop), "monitor:N" (1-based),
    /// "window" + `title`/`hwnd`, or an explicit `region`.
    target: Option<String>,
    /// Region [x, y, w, h] in screen pixels. Overrides `target` when given.
    region: Option<Vec<i32>>,
    /// Window title substring (case-insensitive) for window/tree/screenshot targets.
    title: Option<String>,
    /// Exact window handle (from `windows`).
    hwnd: Option<i64>,

    // ---- screenshot tuning (same budget model as the image tool) ----
    max_bytes: Option<usize>,
    max_dim: Option<u32>,
    scale: Option<f64>,
    format: Option<String>,
    /// screenshot: zoom one grid tile "CxR:K" of the capture (same meaning as the image tool).
    tile: Option<String>,

    // ---- ocr ----
    /// OCR language tag, e.g. "en" or "ru" (default: system/best installed).
    lang: Option<String>,
    /// ocr: also return each line's bounding box (screen pixels). Default true.
    boxes: Option<bool>,

    // ---- tree ----
    /// tree: max depth to walk (default 12).
    depth: Option<u32>,
    /// tree: only interactive/named elements (buttons, inputs, links, menu items...). Default true.
    interactive_only: Option<bool>,
    /// tree: keep elements whose name/value contains this (case-insensitive).
    filter: Option<String>,
    /// tree/elements: max elements returned (default 400).
    limit: Option<usize>,

    // ---- mouse ----
    /// Mouse x in screen pixels (click/move/drag/scroll origin).
    x: Option<i32>,
    /// Mouse y in screen pixels.
    y: Option<i32>,
    /// drag: destination x.
    to_x: Option<i32>,
    /// drag: destination y.
    to_y: Option<i32>,
    /// click/drag button: left (default) | right | middle.
    button: Option<String>,
    /// click count: 1 (default), 2 = double-click.
    count: Option<u32>,
    /// scroll: vertical notches (+ up / - down).
    dy: Option<i32>,
    /// scroll: horizontal notches (+ right / - left).
    dx: Option<i32>,

    // ---- keyboard ----
    /// type: literal text to type (Unicode).
    text: Option<String>,
    /// type: auto (default: paste long text, key-by-key otherwise) | keys | paste (via clipboard, restores it).
    mode: Option<String>,
    /// key: a chord/sequence like "ctrl+s", "alt+F4", "win+r", "enter", "ctrl+shift+esc".
    keys: Option<String>,

    // ---- window ----
    /// window sub-op: focus (default) | minimize | maximize | restore | close | move.
    op: Option<String>,
    /// window move: new [x, y, w, h] (any of w/h may be omitted by passing the current size).
    rect: Option<Vec<i32>>,

    // ---- clipboard ----
    /// clipboard: "get" (default) or text to set.
    value: Option<String>,
    /// clipboard: set the clipboard to this text (alias for a non-"get" value).
    set: Option<String>,
}

const DESC: &str = "The desktop. Actions: \
screenshot {target?: screen|monitor:N|window, title?/hwnd?, region?, scale?, max_bytes?} (screen pixels; coordinates map back to the screen); \
ocr {target?/region?, lang?, boxes?} (text + line boxes in screen pixels); \
tree {target?/region?, depth?, interactive_only?, filter?, limit?} (UI-element tree with roles and screen rects); \
click/move/drag {x, y, to_x?, to_y?, button?, count?}; scroll {x?, y?, dy?, dx?}; \
type {text, mode?: auto|keys|paste, title?/hwnd?}; key {keys: e.g. \"ctrl+s\", \"alt+F4\", \"win+r\"}; \
windows (list top-level windows with handles and rects); window {title?/hwnd?, op: focus|minimize|maximize|restore|close|move, rect?}; \
clipboard {value?|set?} (get or set text). Pair screenshot/ocr/tree to see, then click/type by the reported coordinates.";

pub fn def() -> ToolDef {
    tool::<UiArgs, _, _>("ui", "Desktop", DESC, json!({"destructiveHint": true, "openWorldHint": false}), run)
}

#[cfg(windows)]
async fn run(a: UiArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let action = a.action.trim().to_ascii_lowercase();
    // Everything here touches the desktop/GDI/COM, so run on a blocking worker.
    blocking(move || win::dispatch(&action, a, &ctx)).await
}

#[cfg(not(windows))]
async fn run(a: UiArgs, _ctx: ToolCtx) -> Result<ToolOutput, String> {
    let _ = &a;
    Err("the `ui` desktop tool is currently implemented on Windows only".into())
}
