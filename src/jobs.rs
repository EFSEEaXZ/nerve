//! Background job registry. Jobs are detached `nerve __run` processes; their metadata, logs and
//! exit codes live on disk, so jobs keep running and stay inspectable across server restarts.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::util::{atomic_write, now_ms};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobMeta {
    pub id: String,
    pub pid: u32,
    pub command: String,
    #[serde(default)]
    pub label: String,
    pub cwd: String,
    pub started: u64,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit_file: PathBuf,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub killed: bool,
    /// "detached" (runner, survives restarts) | "attached" (kept after a timeout, tied to this server)
    #[serde(default)]
    pub kind: String,
}

impl JobMeta {
    pub fn running(&self) -> bool {
        self.finished.is_none()
    }
    pub fn status(&self) -> String {
        if self.finished.is_none() {
            "running".into()
        } else if self.killed {
            "killed".into()
        } else {
            match self.exit_code {
                Some(c) => format!("exited({c})"),
                None => "lost".into(),
            }
        }
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.finished.unwrap_or_else(now_ms).saturating_sub(self.started)
    }
}

pub struct Jobs {
    dir: PathBuf,
    map: Mutex<HashMap<String, JobMeta>>,
    seq: AtomicU64,
}

impl Jobs {
    pub fn load(dir: &Path) -> Self {
        let mut map = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("json") {
                    if let Ok(b) = std::fs::read(&p) {
                        if let Ok(m) = serde_json::from_slice::<JobMeta>(&b) {
                            map.insert(m.id.clone(), m);
                        }
                    }
                }
            }
        }
        Self { dir: dir.to_path_buf(), map: Mutex::new(map), seq: AtomicU64::new(1) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn new_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("j{}-{n}", chrono::Local::now().format("%m%d-%H%M%S"))
    }

    /// (stdout, stderr, exit file)
    pub fn paths(&self, id: &str) -> (PathBuf, PathBuf, PathBuf) {
        (self.dir.join(format!("{id}.out.log")), self.dir.join(format!("{id}.err.log")), self.dir.join(format!("{id}.exit")))
    }

    fn persist(&self, m: &JobMeta) {
        let _ = atomic_write(&self.dir.join(format!("{}.json", m.id)), serde_json::to_string_pretty(m).unwrap_or_default().as_bytes());
    }

    pub fn insert(&self, m: JobMeta) {
        self.persist(&m);
        self.map.lock().unwrap().insert(m.id.clone(), m);
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut JobMeta)) {
        let mut map = self.map.lock().unwrap();
        if let Some(m) = map.get_mut(id) {
            f(m);
            let m = m.clone();
            drop(map);
            self.persist(&m);
        }
    }

    fn refresh_one(&self, m: &mut JobMeta) -> bool {
        if m.finished.is_some() {
            return false;
        }
        if let Ok(s) = std::fs::read_to_string(&m.exit_file) {
            m.exit_code = s.trim().parse().ok();
            m.finished = Some(std::fs::metadata(&m.exit_file).and_then(|x| x.modified()).map(crate::util::to_ms).unwrap_or_else(|_| now_ms()));
            return true;
        }
        if m.pid != 0 && !crate::util::pid_alive(m.pid) {
            // re-check: the exit file may have been written in between
            if let Ok(s) = std::fs::read_to_string(&m.exit_file) {
                m.exit_code = s.trim().parse().ok();
            }
            m.finished = Some(now_ms());
            return true;
        }
        false
    }

    pub fn get(&self, id: &str) -> Option<JobMeta> {
        let mut map = self.map.lock().unwrap();
        let m = map.get_mut(id)?;
        let changed = self.refresh_one(m);
        let m = m.clone();
        drop(map);
        if changed {
            self.persist(&m);
        }
        Some(m)
    }

    pub fn list(&self) -> Vec<JobMeta> {
        let ids: Vec<String> = self.map.lock().unwrap().keys().cloned().collect();
        let mut v: Vec<JobMeta> = ids.iter().filter_map(|id| self.get(id)).collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.started));
        v
    }

    pub fn running_count(&self) -> usize {
        self.map.lock().unwrap().values().filter(|m| m.finished.is_none()).count()
    }

    pub fn remove(&self, id: &str, delete_logs: bool) -> bool {
        let m = self.map.lock().unwrap().remove(id);
        if let Some(m) = m {
            let _ = std::fs::remove_file(self.dir.join(format!("{id}.json")));
            let _ = std::fs::remove_file(&m.exit_file);
            if delete_logs {
                let _ = std::fs::remove_file(&m.stdout);
                let _ = std::fs::remove_file(&m.stderr);
            }
            true
        } else {
            false
        }
    }
}

/// Read the last `n` lines of a file without loading it whole. Returns (text, file size).
pub fn tail_lines(path: &Path, n: usize, max_bytes: usize) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    if size == 0 || n == 0 {
        return Ok((String::new(), size));
    }
    let mut pos = size;
    let mut buf: Vec<u8> = Vec::new();
    let chunk = 64 * 1024u64;
    loop {
        let start = pos.saturating_sub(chunk);
        let len = (pos - start) as usize;
        f.seek(SeekFrom::Start(start))?;
        let mut part = vec![0u8; len];
        f.read_exact(&mut part)?;
        part.extend_from_slice(&buf);
        buf = part;
        pos = start;
        let nl = memchr::memchr_iter(b'\n', &buf).count();
        if nl > n || pos == 0 || buf.len() >= max_bytes {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.trim_end_matches(['\n', '\r']).split('\n').collect();
    let from = lines.len().saturating_sub(n);
    let mut out = lines[from..].join("\n").replace('\r', "");
    if out.len() > max_bytes {
        let cut = out.len() - max_bytes;
        let cut = (cut..out.len()).find(|i| out.is_char_boundary(*i)).unwrap_or(out.len());
        out = format!("…{}", &out[cut..]);
    }
    Ok((out, size))
}

/// Read bytes from `offset` to EOF (capped). Returns (text, next offset).
pub fn read_from(path: &Path, offset: u64, max_bytes: usize) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let start = offset.min(size);
    f.seek(SeekFrom::Start(start))?;
    let want = ((size - start) as usize).min(max_bytes);
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf)?;
    Ok((String::from_utf8_lossy(&buf).replace('\r', ""), start + want as u64))
}