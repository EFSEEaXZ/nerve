//! `fs`: read/write/edit files, list/find/grep trees, and file operations.

mod ops;
mod text;
mod walk;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::mcp::{ToolCtx, ToolDef, ToolOutput, blocking, need, tool};
use crate::util;

#[derive(Deserialize, JsonSchema, Clone)]
pub struct Edit {
    /// Exact text to find (copy it verbatim from the file, including indentation).
    #[serde(alias = "old_string", alias = "old_str", alias = "search")]
    pub old: String,
    /// Replacement text.
    #[serde(alias = "new_string", alias = "new_str", alias = "replace")]
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FsArgs {
    /// read | write | append | edit | list | find | grep | stat | mkdir | copy | move | remove | hash | size
    pub action: String,
    /// File or directory. Relative paths resolve against the default directory; `~` = home.
    #[serde(alias = "file", alias = "file_path", alias = "dir")]
    pub path: Option<String>,
    /// Several paths at once (read, stat, mkdir, remove, hash).
    pub paths: Option<Vec<String>>,
    /// write/append: the text (or base64 data with base64:true).
    pub content: Option<String>,
    /// write/append: `content` is base64-encoded binary data.
    pub base64: Option<bool>,
    /// edit: exact text to replace (must match exactly once unless replace_all).
    #[serde(alias = "old_string", alias = "old_str")]
    pub old: Option<String>,
    /// edit: replacement text.
    #[serde(alias = "new_string", alias = "new_str")]
    pub new: Option<String>,
    /// edit: replace all occurrences of `old`.
    pub replace_all: Option<bool>,
    /// edit: several replacements applied in order, all-or-nothing.
    pub edits: Option<Vec<Edit>>,
    /// read: first line, 1-based (negative = last N lines).
    pub offset: Option<i64>,
    /// read: max lines (default 2000).
    pub limit: Option<usize>,
    /// read: prefix each line with its number.
    pub numbered: Option<bool>,
    /// grep: regex; find: glob like `*.rs` / `src/**/mod.rs`, or a plain name substring.
    #[serde(alias = "query", alias = "regex")]
    pub pattern: Option<String>,
    /// grep: only search files matching these globs (comma-separated, e.g. `*.rs,*.toml`).
    pub glob: Option<String>,
    /// grep: case-insensitive.
    pub ignore_case: Option<bool>,
    /// grep: treat pattern as literal text.
    pub literal: Option<bool>,
    /// grep: lines of context around matches.
    pub context: Option<usize>,
    /// grep: only list matching files.
    pub files_only: Option<bool>,
    /// list/find/grep: max directory depth (list default 1).
    pub depth: Option<usize>,
    /// list/find/grep: include hidden and .gitignored entries.
    pub all: Option<bool>,
    /// find: file | dir
    pub kind: Option<String>,
    /// copy/move: destination path (an existing directory receives the item inside it).
    #[serde(alias = "destination", alias = "to", alias = "target")]
    pub dest: Option<String>,
    /// write: overwrite (default true); copy/move: overwrite existing (default false).
    pub overwrite: Option<bool>,
    /// remove: delete non-empty directories.
    pub recursive: Option<bool>,
    /// list/find/grep/size: max entries (defaults 500/300/200/15).
    pub max_results: Option<usize>,
    /// read: max bytes returned (default 100000).
    pub max_bytes: Option<usize>,
    /// read/write/edit: text encoding (auto-detected; e.g. utf-8, utf-8-bom, utf-16le, windows-1251, cp866). read: `base64` = raw bytes.
    pub encoding: Option<String>,
    /// find/grep/size: stop after this many ms (default 120000).
    pub timeout_ms: Option<u64>,
}

const DESC: &str = "Files and directories. Actions: \
read {path|paths, offset?, limit?, numbered?} (auto-detects UTF-8/UTF-16/BOM/cp1251; negative offset = tail); \
write {path, content, base64?} (creates parent dirs; keeps the file's encoding and CRLF); append; \
edit {path, old, new, replace_all?} or {path, edits:[{old,new}]} — exact-text replacement, all-or-nothing, shows the changed lines; \
list {path, depth?}; find {path, pattern (glob or name part), kind?}; \
grep {path, pattern (regex), glob?, context?, files_only?, ignore_case?, literal?} (parallel, respects .gitignore unless all:true); \
stat; mkdir; copy/move {path, dest, overwrite?}; remove {path|paths, recursive?}; hash (sha256); size (disk usage + largest children).";

pub fn def() -> ToolDef {
    tool::<FsArgs, _, _>("fs", "Files", DESC, json!({"destructiveHint": true, "openWorldHint": false}), run)
}

pub(crate) struct Ctx {
    base: String,
    pub cancel: CancellationToken,
}

impl Ctx {
    pub fn path(&self, p: &str) -> PathBuf {
        util::resolve_path(p, Some(&self.base))
    }
    pub fn one(&self, a: &FsArgs) -> Result<PathBuf, String> {
        Ok(self.path(need(a.path.as_deref(), "path")?))
    }
    pub fn many(&self, a: &FsArgs) -> Result<Vec<PathBuf>, String> {
        let mut v: Vec<PathBuf> = a.paths.iter().flatten().map(|p| self.path(p)).collect();
        if let Some(p) = &a.path {
            v.insert(0, self.path(p));
        }
        if v.is_empty() {
            return Err("missing required parameter `path` (or `paths`)".into());
        }
        Ok(v)
    }
}

async fn run(a: FsArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let c = Ctx { base: ctx.app.default_dir.to_string_lossy().to_string(), cancel: ctx.cancel.clone() };
    blocking(move || match a.action.as_str() {
        "read" | "cat" | "view" => text::read(&c, &a),
        "write" | "create" => text::write(&c, &a, false),
        "append" => text::write(&c, &a, true),
        "edit" | "replace" | "patch" => text::edit(&c, &a),
        "list" | "ls" | "tree" => walk::list(&c, &a),
        "find" | "glob" | "search" => walk::find(&c, &a),
        "grep" | "rg" => walk::grep(&c, &a),
        "size" | "du" => walk::size(&c, &a),
        "stat" | "info" | "exists" => ops::stat(&c, &a),
        "mkdir" => ops::mkdir(&c, &a),
        "copy" | "cp" => ops::copy(&c, &a),
        "move" | "mv" | "rename" => ops::mv(&c, &a),
        "remove" | "rm" | "delete" => ops::remove(&c, &a),
        "hash" | "sha256" => ops::hash(&c, &a),
        other => Err(super::unknown_action(
            "fs",
            other,
            "read, write, append, edit, list, find, grep, stat, mkdir, copy, move, remove, hash, size",
        )),
    })
    .await
}

pub(crate) fn io_err(p: &Path, e: std::io::Error) -> String {
    let hint = match e.kind() {
        std::io::ErrorKind::NotFound => " (not found)",
        std::io::ErrorKind::PermissionDenied => " (permission denied: locked by another program or needs elevation)",
        _ => "",
    };
    format!("{}: {e}{hint}", p.display())
}

pub(crate) fn ts(t: std::io::Result<SystemTime>) -> String {
    match t {
        Ok(t) => chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string(),
        Err(_) => "-".into(),
    }
}

pub(crate) fn rel(root: &Path, p: &Path) -> String {
    let r = p.strip_prefix(root).unwrap_or(p);
    let s = r.display().to_string();
    if s.is_empty() { ".".into() } else { s }
}

pub(crate) fn deadline(a: &FsArgs) -> Instant {
    Instant::now() + Duration::from_millis(a.timeout_ms.unwrap_or(120_000).max(1000))
}