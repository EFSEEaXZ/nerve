//! Tree walking: list, find, grep, size. Parallel, cancellable, time-limited.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ignore::{WalkBuilder, WalkState};

use super::{Ctx, FsArgs, deadline, io_err, rel, ts};
use crate::mcp::{OrErr, ToolOutput, need};
use crate::util::{self, fmt_bytes};

fn walker(root: &Path, a: &FsArgs, depth: Option<usize>, filters: bool) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    b.max_depth(depth).follow_links(false);
    if filters && !a.all.unwrap_or(false) {
        b.standard_filters(true).hidden(false).require_git(false).filter_entry(|e| e.file_name() != ".git");
    } else {
        b.standard_filters(false);
    }
    b
}

pub fn list(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let root = c.path(a.path.as_deref().unwrap_or("."));
    let md = std::fs::metadata(&root).map_err(|e| io_err(&root, e))?;
    if !md.is_dir() {
        return super::ops::stat(c, a);
    }
    let depth = a.depth.unwrap_or(1).max(1);
    let max = a.max_results.unwrap_or(500).max(1);
    let mut entries: Vec<(bool, String, u64, String)> = vec![];
    let mut errs = 0usize;
    for e in walker(&root, a, Some(depth), depth > 1).build() {
        let Ok(e) = e else {
            errs += 1;
            continue;
        };
        if e.depth() == 0 {
            continue;
        }
        if c.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let md = e.metadata().ok();
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        let size = if is_dir { 0 } else { md.as_ref().map(|m| m.len()).unwrap_or(0) };
        let mt = md.map(|m| ts(m.modified())).unwrap_or_else(|| "-".into());
        entries.push((is_dir, rel(&root, e.path()), size, mt));
        if entries.len() > 100_000 {
            break;
        }
    }
    if depth == 1 {
        entries.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.to_lowercase().cmp(&y.1.to_lowercase())));
    } else {
        entries.sort_by(|x, y| x.1.to_lowercase().cmp(&y.1.to_lowercase()));
    }
    let dirs = entries.iter().filter(|e| e.0).count();
    let files = entries.len() - dirs;
    let bytes: u64 = entries.iter().map(|e| e.2).sum();
    let sep = std::path::MAIN_SEPARATOR;
    let mut s = format!("{} : {dirs} dirs, {files} files, {}\n", root.display(), fmt_bytes(bytes));
    for (d, name, size, mt) in entries.iter().take(max) {
        if *d {
            s.push_str(&format!("d {:>9}  {mt}  {name}{sep}\n", "-"));
        } else {
            s.push_str(&format!("f {:>9}  {mt}  {name}\n", fmt_bytes(*size)));
        }
    }
    if entries.len() > max {
        s.push_str(&format!("… {} more (raise max_results)\n", entries.len() - max));
    }
    if errs > 0 {
        s.push_str(&format!("({errs} entries unreadable)\n"));
    }
    Ok(ToolOutput::text(s.trim_end().to_string()))
}

fn is_glob(p: &str) -> bool {
    p.contains(['*', '?', '[', '{'])
}

/// Comma-separated globs. Returns (set, whether any glob contains a path separator).
fn build_globs(spec: &str) -> Result<(globset::GlobSet, bool), String> {
    let mut b = globset::GlobSetBuilder::new();
    let mut any_sep = false;
    for g in spec.split(',').map(str::trim).filter(|g| !g.is_empty()) {
        let g = g.replace('\\', "/");
        any_sep |= g.contains('/');
        b.add(globset::GlobBuilder::new(&g).case_insensitive(true).literal_separator(false).build().ctx(format!("bad glob `{g}`"))?);
    }
    Ok((b.build().ctx("glob")?, any_sep))
}

fn glob_hit(g: &(globset::GlobSet, bool), rel: &str, name: &std::ffi::OsStr) -> bool {
    if g.1 { g.0.is_match(rel.replace('\\', "/")) } else { g.0.is_match(name) }
}

pub fn find(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let root = c.path(a.path.as_deref().unwrap_or("."));
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let pat = a.pattern.clone().unwrap_or_default();
    let want: Option<bool> = match a.kind.as_deref() {
        None | Some("") | Some("any") | Some("all") => None,
        Some("file") | Some("f") | Some("files") => Some(false),
        Some("dir") | Some("d") | Some("directory") | Some("dirs") => Some(true),
        Some(o) => return Err(format!("unknown kind `{o}` (file|dir)")),
    };
    let globs = if is_glob(&pat) { Some(build_globs(&pat)?) } else { None };
    let needle = pat.to_lowercase();
    let max = a.max_results.unwrap_or(300).max(1);
    let found = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let until = deadline(a);
    let timed_out = Arc::new(AtomicBool::new(false));
    walker(&root, a, a.depth, true).build_parallel().run(|| {
        let (found, count, root, globs, needle, cancel, timed_out) =
            (found.clone(), count.clone(), root.clone(), globs.clone(), needle.clone(), c.cancel.clone(), timed_out.clone());
        Box::new(move |e| {
            let Ok(e) = e else { return WalkState::Continue };
            if e.depth() == 0 {
                return WalkState::Continue;
            }
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            if Instant::now() > until {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if want.is_some_and(|w| w != is_dir) {
                return WalkState::Continue;
            }
            let r = rel(&root, e.path());
            let ok = match &globs {
                Some(g) => glob_hit(g, &r, e.file_name()),
                None => needle.is_empty() || e.file_name().to_string_lossy().to_lowercase().contains(&needle),
            };
            if ok {
                if count.fetch_add(1, Ordering::Relaxed) >= max {
                    return WalkState::Quit;
                }
                found.lock().unwrap().push((r, is_dir));
            }
            WalkState::Continue
        })
    });
    let mut v = std::mem::take(&mut *found.lock().unwrap());
    v.sort_by(|x, y| x.0.to_lowercase().cmp(&y.0.to_lowercase()));
    let total = count.load(Ordering::Relaxed);
    let sep = std::path::MAIN_SEPARATOR;
    let mut s = format!(
        "{} match{} under {}{}{}\n",
        v.len(),
        if v.len() == 1 { "" } else { "es" },
        root.display(),
        if total > max { format!(" (stopped at max_results={max})") } else { String::new() },
        if timed_out.load(Ordering::Relaxed) { " (time limit reached; partial)" } else { "" }
    );
    for (r, d) in &v {
        s.push_str(r);
        if *d {
            s.push(sep);
        }
        s.push('\n');
    }
    if v.is_empty() && !a.all.unwrap_or(false) {
        s.push_str("(.gitignored entries were skipped; all:true includes them)\n");
    }
    Ok(ToolOutput::text(s.trim_end().to_string()))
}

// ---------------------------------------------------------------- grep

struct GrepSink<'a> {
    lines: Vec<String>,
    matches: usize,
    files_only: bool,
    budget: &'a AtomicUsize,
    max: usize,
    last_line: u64,
}

impl GrepSink<'_> {
    fn push(&mut self, ln: Option<u64>, bytes: &[u8], sep: char) {
        let ln = ln.unwrap_or(0);
        if !self.lines.is_empty() && self.last_line > 0 && ln > self.last_line + 1 {
            self.lines.push("    --".into());
        }
        self.last_line = ln;
        let t = String::from_utf8_lossy(bytes);
        let t = t.trim_end_matches(['\n', '\r']);
        self.lines.push(format!("{ln:>6}{sep} {}", util::clip_middle(t, 400).replace('\n', " ")));
    }
}

impl grep_searcher::Sink for GrepSink<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _s: &grep_searcher::Searcher, m: &grep_searcher::SinkMatch<'_>) -> Result<bool, std::io::Error> {
        self.matches += 1;
        if self.files_only {
            return Ok(false);
        }
        if self.budget.fetch_add(1, Ordering::Relaxed) >= self.max {
            return Ok(false);
        }
        self.push(m.line_number(), m.bytes(), ':');
        Ok(true)
    }
    fn context(&mut self, _s: &grep_searcher::Searcher, c: &grep_searcher::SinkContext<'_>) -> Result<bool, std::io::Error> {
        if !self.files_only {
            self.push(c.line_number(), c.bytes(), '-');
        }
        Ok(true)
    }
}

type GrepHit = (String, usize, Vec<String>);

pub fn grep(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    use grep_searcher::{BinaryDetection, SearcherBuilder};
    let root = c.path(a.path.as_deref().unwrap_or("."));
    if !root.exists() {
        return Err(io_err(&root, std::io::ErrorKind::NotFound.into()));
    }
    let pat = need(a.pattern.as_deref(), "pattern")?;
    let pat = if a.literal.unwrap_or(false) { regex::escape(pat) } else { pat.to_string() };
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(a.ignore_case.unwrap_or(false))
        .build(&pat)
        .ctx("bad regex (set literal:true to search plain text)")?;
    let globs = match a.glob.as_deref() {
        Some(g) if !g.trim().is_empty() => Some(build_globs(g)?),
        _ => None,
    };
    let ctxn = a.context.unwrap_or(0).min(20);
    let files_only = a.files_only.unwrap_or(false);
    let max = a.max_results.unwrap_or(200).max(1);
    let budget = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(Vec::<GrepHit>::new()));
    let scanned = Arc::new(AtomicUsize::new(0));
    let until = deadline(a);
    let timed_out = Arc::new(AtomicBool::new(false));
    let single = root.is_file();
    walker(&root, a, a.depth, true).build_parallel().run(|| {
        let (matcher, globs, budget, results, scanned, root, cancel, timed_out) = (
            matcher.clone(),
            globs.clone(),
            budget.clone(),
            results.clone(),
            scanned.clone(),
            root.clone(),
            c.cancel.clone(),
            timed_out.clone(),
        );
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .before_context(ctxn)
            .after_context(ctxn)
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build();
        Box::new(move |e| {
            let Ok(e) = e else { return WalkState::Continue };
            if !e.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            if Instant::now() > until {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            if !files_only && budget.load(Ordering::Relaxed) >= max {
                return WalkState::Quit;
            }
            let r = if single { e.path().display().to_string() } else { rel(&root, e.path()) };
            if let Some(g) = &globs {
                if !single && !glob_hit(g, &r, e.file_name()) {
                    return WalkState::Continue;
                }
            }
            scanned.fetch_add(1, Ordering::Relaxed);
            let mut sink = GrepSink { lines: vec![], matches: 0, files_only, budget: &budget, max, last_line: 0 };
            if searcher.search_path(&matcher, e.path(), &mut sink).is_ok() && sink.matches > 0 {
                let mut res = results.lock().unwrap();
                res.push((r, sink.matches, sink.lines));
                if files_only && res.len() >= max {
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });
    let mut res = std::mem::take(&mut *results.lock().unwrap());
    res.sort_by(|x, y| x.0.to_lowercase().cmp(&y.0.to_lowercase()));
    let shown: usize = res.iter().map(|r| r.2.iter().filter(|l| l.as_bytes().get(6) == Some(&b':')).count()).sum();
    let limited = if files_only { res.len() >= max } else { budget.load(Ordering::Relaxed) >= max };
    let pl = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else if w.ends_with('h') { "es" } else { "s" });
    let mut s = if files_only { format!("{} with matches", pl(res.len(), "file")) } else { format!("{} in {}", pl(shown, "match"), pl(res.len(), "file")) };
    s.push_str(&format!(" ({} searched", pl(scanned.load(Ordering::Relaxed), "file")));
    if limited {
        s.push_str(&format!("; stopped at max_results={max}"));
    }
    if timed_out.load(Ordering::Relaxed) {
        s.push_str("; time limit reached, partial");
    }
    s.push_str(")\n");
    for (r, _, lines) in &res {
        s.push_str(r);
        s.push('\n');
        if !files_only {
            for l in lines {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    if res.is_empty() && !a.all.unwrap_or(false) && !single {
        s.push_str("(binary and .gitignored files were skipped; all:true includes ignored files)\n");
    }
    Ok(ToolOutput::text(s.trim_end().to_string()))
}

// ---------------------------------------------------------------- size

type DuAgg = (u64, u64, HashMap<String, u64>);

struct DuLocal {
    files: u64,
    bytes: u64,
    by_child: HashMap<String, u64>,
    shared: Arc<Mutex<DuAgg>>,
}

impl Drop for DuLocal {
    fn drop(&mut self) {
        let mut g = self.shared.lock().unwrap();
        g.0 += self.files;
        g.1 += self.bytes;
        for (k, v) in self.by_child.drain() {
            *g.2.entry(k).or_default() += v;
        }
    }
}

pub fn size(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let root = c.path(a.path.as_deref().unwrap_or("."));
    let md = std::fs::metadata(&root).map_err(|e| io_err(&root, e))?;
    if md.is_file() {
        return Ok(ToolOutput::text(format!("{}: {} ({} B)", root.display(), fmt_bytes(md.len()), md.len())));
    }
    let shared: Arc<Mutex<DuAgg>> = Arc::new(Mutex::new((0, 0, HashMap::new())));
    let until = deadline(a);
    let timed_out = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let mut b = WalkBuilder::new(&root);
    b.standard_filters(false).follow_links(false);
    b.build_parallel().run(|| {
        let mut local = DuLocal { files: 0, bytes: 0, by_child: HashMap::new(), shared: shared.clone() };
        let (root, cancel, timed_out) = (root.clone(), c.cancel.clone(), timed_out.clone());
        Box::new(move |e| {
            let Ok(e) = e else { return WalkState::Continue };
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            if Instant::now() > until {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            if e.file_type().is_some_and(|t| t.is_file()) {
                let n = e.metadata().map(|m| m.len()).unwrap_or(0);
                local.files += 1;
                local.bytes += n;
                if let Ok(r) = e.path().strip_prefix(&root) {
                    let mut comps = r.components();
                    if let Some(first) = comps.next() {
                        let mut key = first.as_os_str().to_string_lossy().to_string();
                        if comps.next().is_some() {
                            key.push(std::path::MAIN_SEPARATOR);
                        }
                        *local.by_child.entry(key).or_default() += n;
                    }
                }
            }
            WalkState::Continue
        })
    });
    let (files, bytes, by) = std::mem::take(&mut *shared.lock().unwrap());
    let mut v: Vec<(String, u64)> = by.into_iter().collect();
    v.sort_by(|x, y| y.1.cmp(&x.1));
    let top = a.max_results.unwrap_or(15);
    let mut s = format!(
        "{}: {} in {files} files ({}){}\nlargest:\n",
        root.display(),
        fmt_bytes(bytes),
        util::fmt_dur_ms(t0.elapsed().as_millis() as u64),
        if timed_out.load(Ordering::Relaxed) { "; time limit reached, partial" } else { "" }
    );
    for (k, n) in v.iter().take(top) {
        let pct = if bytes > 0 { *n as f64 * 100.0 / bytes as f64 } else { 0.0 };
        s.push_str(&format!("{:>9} {pct:>5.1}%  {k}\n", fmt_bytes(*n)));
    }
    Ok(ToolOutput::text(s.trim_end().to_string()))
}