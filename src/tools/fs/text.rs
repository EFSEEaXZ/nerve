//! Text files: encoding detection, read, write/append, exact-text edit.

use std::io::{BufRead, Read, Write};
use std::path::Path;

use super::{Ctx, Edit, FsArgs, io_err};
use crate::mcp::{ToolOutput, need};
use crate::util::{self, fmt_bytes};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Utf8,
    Utf16Le,
    Utf16Be,
    Legacy(&'static encoding_rs::Encoding),
}

#[derive(Clone, Copy, PartialEq)]
pub struct Enc {
    pub kind: Kind,
    pub bom: bool,
}

impl Enc {
    pub const UTF8: Enc = Enc { kind: Kind::Utf8, bom: false };
    pub fn name(&self) -> String {
        match self.kind {
            Kind::Utf8 => if self.bom { "utf-8 with BOM".into() } else { "utf-8".into() },
            Kind::Utf16Le => "utf-16le".into(),
            Kind::Utf16Be => "utf-16be".into(),
            Kind::Legacy(e) => e.name().to_ascii_lowercase(),
        }
    }
}

pub fn parse_enc(label: &str) -> Result<Enc, String> {
    let l = label.trim().to_ascii_lowercase().replace('_', "-");
    Ok(match l.as_str() {
        "utf-8" | "utf8" => Enc::UTF8,
        "utf-8-bom" | "utf8-bom" | "utf-8bom" | "utf-8-sig" => Enc { kind: Kind::Utf8, bom: true },
        "utf-16" | "utf-16le" | "utf16" | "utf16le" | "unicode" => Enc { kind: Kind::Utf16Le, bom: true },
        "utf-16be" | "utf16be" => Enc { kind: Kind::Utf16Be, bom: true },
        "cp866" | "866" | "dos" | "oem" => Enc { kind: Kind::Legacy(encoding_rs::IBM866), bom: false },
        _ => match encoding_rs::Encoding::for_label(l.as_bytes()) {
            Some(e) => Enc { kind: Kind::Legacy(e), bom: false },
            None => return Err(format!("unknown encoding `{label}`")),
        },
    })
}

fn utf16_guess(b: &[u8]) -> Option<Kind> {
    let n = b.len().min(4096) & !1;
    if n < 4 {
        return None;
    }
    let (mut z_even, mut z_odd) = (0, 0);
    for i in (0..n).step_by(2) {
        if b[i] == 0 {
            z_even += 1;
        }
        if b[i + 1] == 0 {
            z_odd += 1;
        }
    }
    let pairs = n / 2;
    if z_odd * 10 > pairs * 4 && z_even * 20 < pairs {
        Some(Kind::Utf16Le)
    } else if z_even * 10 > pairs * 4 && z_odd * 20 < pairs {
        Some(Kind::Utf16Be)
    } else {
        None
    }
}

/// Guess a legacy 8-bit encoding for non-UTF-8 text (Cyrillic-aware).
pub fn legacy_guess(b: &[u8]) -> &'static encoding_rs::Encoding {
    let (mut hi, mut cyr1251, mut cyr866) = (0usize, 0usize, 0usize);
    for &x in b.iter().take(1 << 20) {
        if x >= 0x80 {
            hi += 1;
            if x >= 0xC0 || x == 0xA8 || x == 0xB8 {
                cyr1251 += 1;
            }
            if (0x80..=0xAF).contains(&x) || (0xE0..=0xF1).contains(&x) {
                cyr866 += 1;
            }
        }
    }
    if hi == 0 || cyr1251 * 10 >= hi * 7 {
        encoding_rs::WINDOWS_1251
    } else if cyr866 * 10 >= hi * 8 {
        encoding_rs::IBM866
    } else {
        encoding_rs::WINDOWS_1252
    }
}

fn decode_with(b: &[u8], e: Enc) -> String {
    match e.kind {
        Kind::Utf8 => String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(b)).into_owned(),
        Kind::Utf16Le | Kind::Utf16Be => {
            let le = e.kind == Kind::Utf16Le;
            let bom: &[u8] = if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] };
            let b = b.strip_prefix(bom).unwrap_or(b);
            let u: Vec<u16> = b
                .chunks_exact(2)
                .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                .collect();
            String::from_utf16_lossy(&u)
        }
        Kind::Legacy(enc) => enc.decode_without_bom_handling(b).0.into_owned(),
    }
}

/// Decode bytes as text. `Err(())` = binary.
pub fn decode(b: &[u8], forced: Option<Enc>) -> Result<(String, Enc), ()> {
    if let Some(e) = forced {
        return Ok((decode_with(b, e), e));
    }
    let e = if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Enc { kind: Kind::Utf8, bom: true }
    } else if b.starts_with(&[0xFF, 0xFE]) {
        Enc { kind: Kind::Utf16Le, bom: true }
    } else if b.starts_with(&[0xFE, 0xFF]) {
        Enc { kind: Kind::Utf16Be, bom: true }
    } else if let Some(k) = utf16_guess(b) {
        Enc { kind: k, bom: false }
    } else if util::looks_binary(b) {
        return Err(());
    } else if std::str::from_utf8(b).is_ok() {
        Enc::UTF8
    } else {
        Enc { kind: Kind::Legacy(legacy_guess(b)), bom: false }
    };
    Ok((decode_with(b, e), e))
}

pub fn encode(s: &str, e: Enc) -> Result<Vec<u8>, String> {
    Ok(match e.kind {
        Kind::Utf8 => {
            let mut v = Vec::with_capacity(s.len() + 3);
            if e.bom {
                v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            v.extend_from_slice(s.as_bytes());
            v
        }
        Kind::Utf16Le | Kind::Utf16Be => {
            let le = e.kind == Kind::Utf16Le;
            let mut v = Vec::with_capacity(s.len() * 2 + 2);
            if e.bom {
                v.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for u in s.encode_utf16() {
                v.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            v
        }
        Kind::Legacy(enc) => {
            let (out, _, bad) = enc.encode(s);
            if bad {
                return Err(format!(
                    "text has characters that {} cannot represent; pass encoding:\"utf-8\" to convert the file",
                    e.name()
                ));
            }
            out.into_owned()
        }
    })
}

fn forced_enc(a: &FsArgs) -> Result<Option<Enc>, String> {
    match a.encoding.as_deref() {
        None | Some("") | Some("auto") | Some("base64") | Some("binary") => Ok(None),
        Some(l) => parse_enc(l).map(Some),
    }
}

// ---------------------------------------------------------------- read

const BIG_FILE: u64 = 48 << 20;
const MAX_LINE: usize = 8000;

pub fn read(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let paths = c.many(a)?;
    let budget = a.max_bytes.unwrap_or(100_000).max(1000);
    let per = budget / paths.len();
    let mut out = String::new();
    let mut errors = 0;
    for p in &paths {
        if paths.len() > 1 {
            out.push_str(&format!("==> {} <==\n", p.display()));
        }
        match read_one(p, a, per) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                errors += 1;
                out.push_str(&format!("error: {e}"));
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(ToolOutput::text(out.trim_end_matches('\n').to_string()).error_if(errors == paths.len()))
}

fn clip_line(l: &str) -> String {
    if l.len() > MAX_LINE { util::clip_middle(l, MAX_LINE) } else { l.to_string() }
}

struct Lines {
    out: String,
    shown: usize,
    budget: usize,
    numbered: bool,
}

impl Lines {
    /// false = budget exhausted
    fn push(&mut self, i: usize, l: &str) -> bool {
        let l = clip_line(l.strip_suffix('\r').unwrap_or(l));
        let add = if self.numbered { format!("{i:>6}\t{l}\n") } else { format!("{l}\n") };
        if self.out.len() + add.len() > self.budget && self.shown > 0 {
            return false;
        }
        self.out.push_str(&add);
        self.shown += 1;
        true
    }
}

fn read_one(p: &Path, a: &FsArgs, budget: usize) -> Result<String, String> {
    let md = std::fs::metadata(p).map_err(|e| io_err(p, e))?;
    if md.is_dir() {
        return Err(format!("{} is a directory (use action list)", p.display()));
    }
    let size = md.len();
    let off = a.offset.unwrap_or(0);
    if matches!(a.encoding.as_deref(), Some("base64") | Some("binary")) {
        let start = off.max(0) as u64;
        let mut f = std::fs::File::open(p).map_err(|e| io_err(p, e))?;
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start)).map_err(|e| io_err(p, e))?;
        let want = ((budget / 4) * 3).min(size.saturating_sub(start) as usize);
        let mut buf = vec![0u8; want];
        f.read_exact(&mut buf).map_err(|e| io_err(p, e))?;
        let end = start + want as u64;
        let next = if end < size { format!("; next offset={end}") } else { String::new() };
        return Ok(format!("[bytes {start}..{end} of {size}, base64{next}]\n{}", util::b64(&buf)));
    }
    if off < 0 {
        let n = off.unsigned_abs() as usize;
        let (t, size) = crate::jobs::tail_lines(p, n, budget).map_err(|e| io_err(p, e))?;
        let t: String = t.lines().map(|l| clip_line(l) + "\n").collect();
        return Ok(format!("{t}[last {n} lines; file {}]", fmt_bytes(size)));
    }
    let start = off.max(1) as usize;
    let limit = a.limit.unwrap_or(2000).max(1);
    let forced = forced_enc(a)?;
    let mut ls = Lines { out: String::new(), shown: 0, budget, numbered: a.numbered.unwrap_or(false) };
    let mut total: Option<usize> = None;
    let mut cut = false;
    let mut note = String::new();
    if size > BIG_FILE && forced.is_none() {
        // stream huge files line by line (UTF-8 assumed)
        let f = std::fs::File::open(p).map_err(|e| io_err(p, e))?;
        let mut i = 0usize;
        for l in std::io::BufReader::with_capacity(1 << 20, f).split(b'\n') {
            let l = l.map_err(|e| io_err(p, e))?;
            i += 1;
            if i < start {
                continue;
            }
            if ls.shown >= limit || !ls.push(i, &String::from_utf8_lossy(&l)) {
                cut = true;
                break;
            }
        }
        if !cut {
            total = Some(i);
        }
    } else {
        let bytes = std::fs::read(p).map_err(|e| io_err(p, e))?;
        let Ok((text, enc)) = decode(&bytes, forced) else {
            let ext = p.extension().map(|e| format!(", .{}", e.to_string_lossy())).unwrap_or_default();
            return Err(format!(
                "{} is binary ({}{ext}); use the image tool for pictures, encoding:\"base64\" for raw bytes, or hash",
                p.display(),
                fmt_bytes(size)
            ));
        };
        if enc != Enc::UTF8 {
            note = format!("; encoding {}", enc.name());
        }
        if text.contains("\r\n") {
            note.push_str("; CRLF");
        }
        let body = text.strip_suffix('\n').unwrap_or(&text);
        let lines: Vec<&str> = if body.is_empty() { vec![] } else { body.split('\n').collect() };
        if lines.is_empty() {
            return Ok(format!("[empty file{note}]"));
        }
        total = Some(lines.len());
        for (k, l) in lines.iter().enumerate().skip(start - 1) {
            if ls.shown >= limit || !ls.push(k + 1, l) {
                cut = true;
                break;
            }
        }
    }
    if ls.shown == 0 && start > 1 {
        return Ok(format!("[offset {start} is past the end of file ({} lines)]", total.unwrap_or(0)));
    }
    let end = start + ls.shown.saturating_sub(1);
    let mut out = ls.out;
    if cut || start > 1 || !note.is_empty() {
        let tot = total.map(|t| format!(" of {t}")).unwrap_or_default();
        let next = if cut { format!("; next offset={}", end + 1) } else { String::new() };
        out.push_str(&format!("[lines {start}-{end}{tot}{next}{note}]"));
    }
    Ok(out)
}

// ---------------------------------------------------------------- write / edit

/// (LF count, CRLF count)
fn line_endings(t: &str) -> (usize, usize) {
    (memchr::memchr_iter(b'\n', t.as_bytes()).count(), t.matches("\r\n").count())
}

pub fn write(c: &Ctx, a: &FsArgs, append: bool) -> Result<ToolOutput, String> {
    let p = c.one(a)?;
    if a.content.is_none() && !append {
        return Err("missing required parameter `content` (use \"\" for an empty file)".into());
    }
    let content = a.content.as_deref().unwrap_or("");
    if p.is_dir() {
        return Err(format!("{} is a directory", p.display()));
    }
    let exists = p.exists();
    if exists && !append && a.overwrite == Some(false) {
        return Err(format!("{} already exists (overwrite:false)", p.display()));
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    }
    let binary = a.base64.unwrap_or(false);
    let mut notes: Vec<String> = vec![];
    let data: Vec<u8> = if binary {
        util::b64_decode(content.trim())?
    } else {
        let mut enc = forced_enc(a)?;
        let mut text = content.to_string();
        if exists && std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) <= BIG_FILE {
            if let Ok(old) = std::fs::read(&p) {
                if let Ok((old_text, old_enc)) = decode(&old, None) {
                    if enc.is_none() && old_enc != Enc::UTF8 {
                        enc = Some(old_enc);
                        notes.push(format!("kept encoding {}", old_enc.name()));
                    }
                    let (lf, crlf) = line_endings(&old_text);
                    if lf > 0 && crlf == lf && !text.contains('\r') && text.contains('\n') {
                        text = text.replace('\n', "\r\n");
                        notes.push("kept CRLF line endings".into());
                    }
                }
            }
        }
        let mut e = enc.unwrap_or(Enc::UTF8);
        if append && exists {
            e.bom = false;
        }
        encode(&text, e)?
    };
    if append {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).map_err(|e| io_err(&p, e))?;
        f.write_all(&data).map_err(|e| io_err(&p, e))?;
    } else {
        util::write_file(&p, &data).map_err(|e| io_err(&p, e))?;
    }
    let lines = if binary { String::new() } else { format!(", {} lines", content.lines().count()) };
    let verb = if append { "appended to" } else if exists { "overwrote" } else { "created" };
    let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(", ")) };
    Ok(ToolOutput::text(format!("{verb} {} ({}{lines}){notes}", p.display(), fmt_bytes(data.len() as u64))))
}

fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_of(text: &str, byte: usize) -> usize {
    memchr::memchr_iter(b'\n', &text.as_bytes()[..byte]).count() + 1
}

fn not_found_hint(text: &str, old: &str) -> String {
    let mut hint = String::new();
    if squash_ws(text).contains(&squash_ws(old)) {
        hint.push_str(" It matches if whitespace is ignored: check indentation (tabs vs spaces) and trailing spaces.");
    }
    if let Some(first) = old.lines().map(str::trim).find(|l| l.len() >= 3) {
        let hits: Vec<String> =
            text.lines().enumerate().filter(|(_, l)| l.contains(first)).take(6).map(|(i, _)| (i + 1).to_string()).collect();
        if hits.is_empty() {
            hint.push_str(" Not even its first line occurs in the file; re-read the file.");
        } else {
            hint.push_str(&format!(
                " Its first line `{}` occurs at line(s) {}; re-read those lines and copy them exactly.",
                util::clip_middle(first, 80),
                hits.join(", ")
            ));
        }
    }
    hint
}

pub fn edit(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let p = c.one(a)?;
    let bytes = std::fs::read(&p).map_err(|e| io_err(&p, e))?;
    let (text, enc) = decode(&bytes, forced_enc(a)?).map_err(|_| format!("{} is binary; cannot edit as text", p.display()))?;
    let (lf, crlf) = line_endings(&text);
    let pure_crlf = lf > 0 && crlf == lf;
    let original = if pure_crlf { text.replace("\r\n", "\n") } else { text.clone() };
    let mut work = original.clone();
    let edits: Vec<Edit> = match &a.edits {
        Some(e) if !e.is_empty() => e.clone(),
        _ => vec![Edit { old: need(a.old.clone(), "old")?, new: need(a.new.clone(), "new")?, replace_all: a.replace_all }],
    };
    let mut report = vec![];
    let mut first_change: Option<(usize, usize)> = None;
    for (i, e) in edits.iter().enumerate() {
        let tag = if edits.len() > 1 { format!("edit #{}: ", i + 1) } else { String::new() };
        let (mut old, mut new) = (e.old.clone(), e.new.clone());
        if pure_crlf || crlf == 0 {
            old = old.replace("\r\n", "\n");
            new = new.replace("\r\n", "\n");
        }
        if old.is_empty() {
            if work.is_empty() {
                work = new.clone();
                report.push(format!("{tag}filled empty file"));
                first_change.get_or_insert((1, new.lines().count()));
                continue;
            }
            return Err(format!("{tag}`old` is empty; give the exact text to replace"));
        }
        let mut count = work.matches(old.as_str()).count();
        if count == 0 && !pure_crlf && crlf > 0 && old.contains('\n') {
            // mixed line endings: retry with the other style
            let alt = if old.contains("\r\n") {
                (old.replace("\r\n", "\n"), new.replace("\r\n", "\n"))
            } else {
                (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
            };
            let c2 = work.matches(alt.0.as_str()).count();
            if c2 > 0 {
                (old, new) = alt;
                count = c2;
            }
        }
        if count == 0 {
            return Err(format!(
                "{tag}`old` text not found in {}; nothing was changed.{}",
                p.display(),
                not_found_hint(&work, &old)
            ));
        }
        let all = e.replace_all.unwrap_or(false);
        if count > 1 && !all {
            let lines: Vec<String> = work.match_indices(old.as_str()).take(8).map(|(b, _)| line_of(&work, b).to_string()).collect();
            return Err(format!(
                "{tag}`old` matches {count} times (lines {}); add surrounding lines to make it unique or set replace_all:true. Nothing was changed.",
                lines.join(", ")
            ));
        }
        let pos = work.find(old.as_str()).unwrap_or(0);
        let line = line_of(&work, pos);
        work = if all { work.replace(old.as_str(), &new) } else { work.replacen(old.as_str(), &new, 1) };
        first_change.get_or_insert((line, new.matches('\n').count() + 1));
        let n = if all { count } else { 1 };
        report.push(format!("{tag}{n} replacement{} at line {line}", if n > 1 { "s" } else { "" }));
    }
    if work == original {
        return Ok(ToolOutput::text(format!("no changes: replacement is identical ({})", p.display())));
    }
    let out = if pure_crlf { work.replace('\n', "\r\n") } else { work.clone() };
    let data = encode(&out, enc)?;
    util::write_file(&p, &data).map_err(|e| io_err(&p, e))?;
    // show the changed region so the caller can verify without re-reading
    let mut s = format!("edited {}: {}", p.display(), report.join("; "));
    if let Some((line, n)) = first_change {
        let lines: Vec<&str> = work.split('\n').collect();
        let from = line.saturating_sub(3);
        let to = (line + n + 1).min(lines.len()).min(from + 40);
        s.push('\n');
        for (k, l) in lines.iter().enumerate().take(to).skip(from) {
            s.push_str(&format!("{:>6}\t{}\n", k + 1, clip_line(l.strip_suffix('\r').unwrap_or(l))));
        }
    }
    Ok(ToolOutput::text(s.trim_end().to_string()))
}