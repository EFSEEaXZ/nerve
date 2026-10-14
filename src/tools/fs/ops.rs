//! File operations: stat, mkdir, copy, move, remove, hash.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{Ctx, FsArgs, io_err, ts};
use crate::mcp::{ToolOutput, need};
use crate::util::{self, fmt_bytes};

pub fn stat(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let paths = c.many(a)?;
    let mut s = String::new();
    let mut arr = vec![];
    for p in &paths {
        match std::fs::symlink_metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                s.push_str(&format!("{}: does not exist\n", p.display()));
                arr.push(json!({"path": p, "exists": false}));
            }
            Err(e) => s.push_str(&format!("{}\n", io_err(p, e))),
            Ok(lm) => {
                let link = if lm.file_type().is_symlink() { std::fs::read_link(p).ok() } else { None };
                let md = std::fs::metadata(p).unwrap_or(lm);
                let kind = if md.is_dir() { "dir" } else if md.is_file() { "file" } else { "other" };
                let mut line = format!("{}: {kind}", p.display());
                if md.is_file() {
                    line.push_str(&format!(", {} ({} B)", fmt_bytes(md.len()), md.len()));
                } else if md.is_dir() {
                    let n = std::fs::read_dir(p).map(|r| r.count()).unwrap_or(0);
                    line.push_str(&format!(", {n} entries"));
                }
                line.push_str(&format!(", modified {}, created {}", ts(md.modified()), ts(md.created())));
                if md.permissions().readonly() {
                    line.push_str(", read-only");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    let at = md.file_attributes();
                    if at & 0x2 != 0 {
                        line.push_str(", hidden");
                    }
                    if at & 0x4 != 0 {
                        line.push_str(", system");
                    }
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    line.push_str(&format!(", mode {:o}", md.permissions().mode() & 0o7777));
                }
                if let Some(t) = &link {
                    line.push_str(&format!(", symlink -> {}", t.display()));
                }
                s.push_str(&line);
                s.push('\n');
                arr.push(json!({
                    "path": p, "exists": true, "kind": kind, "size": md.len(),
                    "modified": md.modified().ok().map(util::to_ms),
                }));
            }
        }
    }
    Ok(ToolOutput::text(s.trim_end().to_string()).with_structured(json!({"items": arr})))
}

pub fn mkdir(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let mut s = vec![];
    for p in c.many(a)? {
        std::fs::create_dir_all(&p).map_err(|e| io_err(&p, e))?;
        s.push(format!("ok {}", p.display()));
    }
    Ok(ToolOutput::text(s.join("\n")))
}

fn dest_for(src: &Path, dst: &Path) -> PathBuf {
    if dst.is_dir() {
        if let Some(n) = src.file_name() {
            return dst.join(n);
        }
    }
    dst.to_path_buf()
}

/// (files, bytes, skipped)
type CopyStats = (u64, u64, u64);

fn copy_tree(src: &Path, dst: &Path, overwrite: bool, st: &mut CopyStats, cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let md = std::fs::symlink_metadata(src).map_err(|e| io_err(src, e))?;
    if md.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| io_err(dst, e))?;
        for e in std::fs::read_dir(src).map_err(|e| io_err(src, e))? {
            let e = e.map_err(|e| io_err(src, e))?;
            copy_tree(&e.path(), &dst.join(e.file_name()), overwrite, st, cancel)?;
        }
    } else {
        if dst.exists() && !overwrite {
            st.2 += 1;
            return Ok(());
        }
        let n = std::fs::copy(src, dst).map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
        st.0 += 1;
        st.1 += n;
    }
    Ok(())
}

pub fn copy(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let src = c.one(a)?;
    let dst = dest_for(&src, &c.path(need(a.dest.as_deref(), "dest")?));
    let overwrite = a.overwrite.unwrap_or(false);
    if !src.exists() {
        return Err(io_err(&src, std::io::ErrorKind::NotFound.into()));
    }
    if src.is_dir() && dst.starts_with(&src) {
        return Err("cannot copy a directory into itself".into());
    }
    if src.is_file() && dst.exists() && !overwrite {
        return Err(format!("{} already exists (set overwrite:true)", dst.display()));
    }
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d).map_err(|e| io_err(d, e))?;
    }
    let mut st: CopyStats = (0, 0, 0);
    copy_tree(&src, &dst, overwrite, &mut st, &c.cancel)?;
    let skipped = if st.2 > 0 { format!(", skipped {} existing (overwrite:false)", st.2) } else { String::new() };
    Ok(ToolOutput::text(format!(
        "copied {} -> {} ({} files, {}{skipped})",
        src.display(),
        dst.display(),
        st.0,
        fmt_bytes(st.1)
    )))
}

pub fn mv(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let src = c.one(a)?;
    let dst = dest_for(&src, &c.path(need(a.dest.as_deref(), "dest")?));
    if std::fs::symlink_metadata(&src).is_err() {
        return Err(io_err(&src, std::io::ErrorKind::NotFound.into()));
    }
    if dst.exists() {
        if !a.overwrite.unwrap_or(false) {
            return Err(format!("{} already exists (set overwrite:true)", dst.display()));
        }
        if dst.is_file() {
            let _ = std::fs::remove_file(&dst);
        }
    }
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d).map_err(|e| io_err(d, e))?;
    }
    if std::fs::rename(&src, &dst).is_err() {
        // different volume (or locked): copy, then delete the source
        let mut st: CopyStats = (0, 0, 0);
        copy_tree(&src, &dst, true, &mut st, &c.cancel)?;
        if src.is_dir() { std::fs::remove_dir_all(&src) } else { std::fs::remove_file(&src) }
            .map_err(|e| format!("copied to {} but could not delete the source: {}", dst.display(), io_err(&src, e)))?;
    }
    Ok(ToolOutput::text(format!("moved {} -> {}", src.display(), dst.display())))
}

fn too_dangerous(p: &Path) -> bool {
    let normal = p.components().filter(|c| matches!(c, Component::Normal(_))).count();
    normal < 2 || p == util::home_dir() || p.parent().is_none()
}

fn remove_one(p: &Path, recursive: bool) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(p)?;
    if md.is_dir() {
        if recursive {
            return std::fs::remove_dir_all(p);
        }
        return std::fs::remove_dir(p).map_err(|e| {
            if std::fs::read_dir(p).map(|mut r| r.next().is_some()).unwrap_or(false) {
                std::io::Error::other("directory not empty (set recursive:true)")
            } else {
                e
            }
        });
    }
    std::fs::remove_file(p).or_else(|e| {
        // read-only files: clear the flag and retry
        let mut perm = md.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(p, perm)?;
            std::fs::remove_file(p)
        } else {
            Err(e)
        }
    })
}

pub fn remove(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let mut lines = vec![];
    let mut failed = 0;
    for p in c.many(a)? {
        if too_dangerous(&p) {
            lines.push(format!("refused {}: too close to the filesystem root or home (delete its contents explicitly)", p.display()));
            failed += 1;
            continue;
        }
        match remove_one(&p, a.recursive.unwrap_or(false)) {
            Ok(()) => lines.push(format!("removed {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => lines.push(format!("{}: already absent", p.display())),
            Err(e) => {
                failed += 1;
                lines.push(io_err(&p, e));
            }
        }
    }
    Ok(ToolOutput::text(lines.join("\n")).error_if(failed > 0))
}

pub fn sha256_file(p: &Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n += k as u64;
        h.update(&buf[..k]);
    }
    Ok((h.finalize().iter().map(|b| format!("{b:02x}")).collect(), n))
}

pub fn hash(c: &Ctx, a: &FsArgs) -> Result<ToolOutput, String> {
    let mut s = String::new();
    let mut failed = 0;
    let paths = c.many(a)?;
    for p in &paths {
        match sha256_file(p) {
            Ok((hx, n)) => s.push_str(&format!("{hx}  {}  ({})\n", p.display(), fmt_bytes(n))),
            Err(e) => {
                failed += 1;
                s.push_str(&format!("{}\n", io_err(p, e)));
            }
        }
    }
    Ok(ToolOutput::text(s.trim_end().to_string()).error_if(failed == paths.len()))
}