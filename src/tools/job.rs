//! `job`: inspect and control background jobs started by `exec`.

use std::io::BufRead;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::jobs::{JobMeta, read_from, tail_lines};
use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, need, tool};
use crate::util::{self, fmt_bytes, fmt_dur_ms, fmt_ts, now_ms};

#[derive(Deserialize, JsonSchema)]
pub struct JobArgs {
    /// list | status | logs | wait | kill | clean
    action: String,
    /// Job id (status/logs/wait/kill; optional for clean).
    id: Option<String>,
    /// logs: both (default) | stdout | stderr
    stream: Option<String>,
    /// logs/status/wait: number of last lines to show (default 60).
    tail: Option<usize>,
    /// logs: read stdout (or `stream`) from this byte offset instead of tailing; pass `next_offset` from the previous call to follow new output.
    offset: Option<u64>,
    /// logs: only lines matching this regex.
    grep: Option<String>,
    /// wait: max ms to wait (default 60000, 0 = until the job ends).
    timeout_ms: Option<u64>,
    /// wait: also return as soon as new output matches this regex (e.g. "Listening on").
    until: Option<String>,
    /// list: max jobs to show (default 30). logs: max bytes (default 60000).
    limit: Option<usize>,
    /// clean: also delete log files (default true).
    delete_logs: Option<bool>,
}

const DESC: &str = "Manage background jobs started by exec (background:true or keep_on_timeout). \
Actions: list; status {id}; logs {id, tail?|offset?, stream?, grep?}; \
wait {id, timeout_ms?, until?} (streams progress, returns when finished or `until` regex matches); \
kill {id} (whole process tree); clean {id?} (forget finished jobs and delete their logs).";

pub fn def() -> ToolDef {
    tool::<JobArgs, _, _>("job", "Background jobs", DESC, json!({"destructiveHint": true, "openWorldHint": false}), run)
}

fn line(m: &JobMeta) -> String {
    let what = if m.label.is_empty() { util::one_line(&m.command, 90) } else { format!("[{}] {}", m.label, util::one_line(&m.command, 70)) };
    format!("{}  {:<11} {:>8}  pid {:<6} {}", m.id, m.status(), fmt_dur_ms(m.elapsed_ms()), m.pid, what)
}

fn get(ctx: &ToolCtx, id: &Option<String>) -> Result<JobMeta, String> {
    let id = need(id.as_deref(), "id")?;
    ctx.app.jobs.get(id).ok_or_else(|| {
        let known: Vec<String> = ctx.app.jobs.list().into_iter().take(5).map(|m| m.id).collect();
        format!("no job `{id}`; recent: {}", if known.is_empty() { "none".into() } else { known.join(", ") })
    })
}

fn tails(m: &JobMeta, n: usize, max: usize) -> String {
    let mut s = String::new();
    for (name, p) in [("stdout", &m.stdout), ("stderr", &m.stderr)] {
        if let Ok((t, size)) = tail_lines(p, n, max) {
            if !t.trim().is_empty() {
                s.push_str(&format!("--- {name} (last {n} lines of {}) ---\n{}\n", fmt_bytes(size), t.trim_end()));
            }
        }
    }
    if s.is_empty() { "(no output yet)\n".into() } else { s }
}

fn sizes(m: &JobMeta) -> (u64, u64) {
    let f = |p: &std::path::Path| std::fs::metadata(p).map(|x| x.len()).unwrap_or(0);
    (f(&m.stdout), f(&m.stderr))
}

fn grep_file(path: &std::path::Path, re: &regex::Regex, n: usize) -> Result<(Vec<String>, usize), String> {
    let f = std::fs::File::open(path).ctx(format!("open {}", path.display()))?;
    let mut hits: std::collections::VecDeque<String> = Default::default();
    let mut total = 0;
    for (i, l) in std::io::BufReader::new(f).split(b'\n').enumerate() {
        let Ok(l) = l else { break };
        let l = String::from_utf8_lossy(&l);
        let l = l.trim_end_matches('\r');
        if re.is_match(l) {
            total += 1;
            hits.push_back(format!("{}: {}", i + 1, util::clip_middle(l, 400)));
            if hits.len() > n {
                hits.pop_front();
            }
        }
    }
    Ok((hits.into(), total))
}

async fn run(a: JobArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let jobs = &ctx.app.jobs;
    let tail = a.tail.unwrap_or(60).max(1);
    match a.action.as_str() {
        "list" | "ls" => {
            let all = jobs.list();
            if all.is_empty() {
                return Ok(ToolOutput::text("no jobs"));
            }
            let running = all.iter().filter(|m| m.running()).count();
            let lim = a.limit.unwrap_or(30);
            let mut s = format!("{} jobs ({running} running)\n", all.len());
            for m in all.iter().take(lim) {
                s.push_str(&line(m));
                s.push('\n');
            }
            if all.len() > lim {
                s.push_str(&format!("… {} more (limit)\n", all.len() - lim));
            }
            let st: Vec<_> = all.iter().take(lim).map(|m| json!({"id": m.id, "status": m.status(), "pid": m.pid, "exit_code": m.exit_code, "elapsed_ms": m.elapsed_ms(), "command": m.command, "label": m.label})).collect();
            Ok(ToolOutput::text(s).with_structured(json!({"jobs": st})))
        }
        "status" | "info" => {
            let m = get(&ctx, &a.id)?;
            let (so, se) = sizes(&m);
            let mut s = format!(
                "{}\nstatus: {}  elapsed: {}  started: {}{}\ncommand: {}\ncwd: {}\nstdout: {} ({})\nstderr: {} ({})\n",
                m.id,
                m.status(),
                fmt_dur_ms(m.elapsed_ms()),
                fmt_ts(m.started),
                m.finished.map(|f| format!("  finished: {}", fmt_ts(f))).unwrap_or_default(),
                m.command,
                m.cwd,
                m.stdout.display(),
                fmt_bytes(so),
                m.stderr.display(),
                fmt_bytes(se)
            );
            s.push_str(&tails(&m, a.tail.unwrap_or(15), 20_000));
            Ok(ToolOutput::text(s).with_structured(serde_json::to_value(&m).unwrap_or_default()))
        }
        "logs" | "log" | "output" => {
            let m = get(&ctx, &a.id)?;
            let max = a.limit.unwrap_or(60_000).max(1000);
            let stream = a.stream.as_deref().unwrap_or(if a.offset.is_some() { "stdout" } else { "both" });
            let picks: Vec<(&str, &std::path::Path)> = match stream {
                "stdout" | "out" => vec![("stdout", &m.stdout)],
                "stderr" | "err" => vec![("stderr", &m.stderr)],
                "both" | "all" => vec![("stdout", &m.stdout), ("stderr", &m.stderr)],
                o => return Err(format!("unknown stream `{o}` (stdout|stderr|both)")),
            };
            let mut s = format!("{} · {}\n", m.id, m.status());
            if let Some(g) = &a.grep {
                let re = regex::Regex::new(g).ctx("bad grep regex")?;
                for (name, p) in picks {
                    let p = p.to_path_buf();
                    let re = re.clone();
                    let (hits, total) = crate::mcp::blocking(move || grep_file(&p, &re, tail)).await.unwrap_or_default();
                    s.push_str(&format!("--- {name}: {total} matching lines{} ---\n", if total > hits.len() { format!(", last {}", hits.len()) } else { String::new() }));
                    for h in hits {
                        s.push_str(&h);
                        s.push('\n');
                    }
                }
                return Ok(ToolOutput::text(s));
            }
            if let Some(off) = a.offset {
                if picks.len() != 1 {
                    return Err("offset needs a single stream (stdout or stderr)".into());
                }
                let (name, p) = picks[0];
                let (t, next) = read_from(p, off, max).ctx(format!("read {name}"))?;
                let size = std::fs::metadata(p).map(|x| x.len()).unwrap_or(next);
                s.push_str(&format!("--- {name} bytes {off}..{next} of {size} ---\n{t}"));
                if !t.ends_with('\n') && !t.is_empty() {
                    s.push('\n');
                }
                s.push_str(&format!("next_offset: {next}{}\n", if next < size { " (more available)" } else { "" }));
                return Ok(ToolOutput::text(s).with_structured(json!({"next_offset": next, "size": size, "running": m.running()})));
            }
            let mut st = json!({});
            for (name, p) in picks {
                match tail_lines(p, tail, max) {
                    Ok((t, size)) => {
                        s.push_str(&format!("--- {name} ({}; last {tail} lines; next_offset {size}) ---\n{}\n", fmt_bytes(size), t.trim_end()));
                        st[format!("{name}_size")] = json!(size);
                    }
                    Err(_) => s.push_str(&format!("--- {name}: (no file) ---\n")),
                }
            }
            Ok(ToolOutput::text(s).with_structured(st))
        }
        "wait" => {
            let m = get(&ctx, &a.id)?;
            let tms = a.timeout_ms.unwrap_or(60_000);
            let deadline = (tms > 0).then(|| Instant::now() + Duration::from_millis(tms));
            let re = a.until.as_deref().map(regex::Regex::new).transpose().ctx("bad `until` regex")?;
            let (mut off_o, mut off_e) = sizes(&m);
            let t0 = Instant::now();
            let mut last_progress = Instant::now() - Duration::from_secs(10);
            let mut matched: Option<String> = None;
            let fin = loop {
                let cur = jobs.get(&m.id).ok_or("job disappeared")?;
                if let Some(re) = &re {
                    for (p, off) in [(&cur.stdout, &mut off_o), (&cur.stderr, &mut off_e)] {
                        if let Ok((t, next)) = read_from(p, *off, 4 << 20) {
                            // only consume complete lines
                            let upto = t.rfind('\n').map(|i| i + 1).unwrap_or(0);
                            if let Some(l) = t[..upto].lines().find(|l| re.is_match(l)) {
                                matched = Some(l.to_string());
                            }
                            *off = if upto == t.len() { next } else { *off + t[..upto].len() as u64 };
                        }
                    }
                }
                if !cur.running() || matched.is_some() {
                    break cur;
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break cur;
                }
                if last_progress.elapsed() >= Duration::from_secs(2) {
                    last_progress = Instant::now();
                    let (o, _) = tail_lines(&cur.stdout, 1, 400).unwrap_or_default();
                    ctx.progress.send(format!("{} · {}", fmt_dur_ms(t0.elapsed().as_millis() as u64), util::one_line(&o, 160)), None);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {}
                    _ = ctx.cancel.cancelled() => return Err("wait cancelled (job keeps running)".into()),
                }
            };
            let head = if let Some(l) = &matched {
                format!("matched `until` after {}: {}\n", fmt_dur_ms(t0.elapsed().as_millis() as u64), util::clip_middle(l, 300))
            } else if fin.running() {
                format!("still running after waiting {} (call wait again or check logs)\n", fmt_dur_ms(t0.elapsed().as_millis() as u64))
            } else {
                String::new()
            };
            let s = format!("{head}{}\n{}", line(&fin), tails(&fin, a.tail.unwrap_or(30), 30_000));
            let err = !fin.running() && fin.exit_code.is_some_and(|c| c != 0);
            Ok(ToolOutput::text(s)
                .with_structured(json!({"id": fin.id, "status": fin.status(), "exit_code": fin.exit_code, "running": fin.running(), "matched": matched}))
                .error_if(err))
        }
        "kill" | "stop" => {
            let m = get(&ctx, &a.id)?;
            if !m.running() {
                return Ok(ToolOutput::text(format!("job {} already {}", m.id, m.status())));
            }
            let pid = m.pid;
            let ok = crate::mcp::blocking(move || Ok(util::kill_tree(pid, true))).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let alive = util::pid_alive(pid);
            if !alive {
                jobs.update(&m.id, |j| {
                    if j.finished.is_none() {
                        j.finished = Some(now_ms());
                        j.killed = true;
                    }
                });
            }
            let msg = if alive { format!("tried to kill job {} (pid {pid}) but it is still alive (kill returned {ok})", m.id) } else { format!("killed job {} (pid {pid} and its children)", m.id) };
            Ok(ToolOutput::text(msg).error_if(alive))
        }
        "clean" | "remove" | "rm" => {
            let del = a.delete_logs.unwrap_or(true);
            if let Some(id) = &a.id {
                let m = get(&ctx, &Some(id.clone()))?;
                if m.running() {
                    return Err(format!("job {id} is still running; kill it first"));
                }
                jobs.remove(id, del);
                return Ok(ToolOutput::text(format!("removed job {id}")));
            }
            let done: Vec<String> = jobs.list().into_iter().filter(|m| !m.running()).map(|m| m.id).collect();
            for id in &done {
                jobs.remove(id, del);
            }
            Ok(ToolOutput::text(format!("removed {} finished jobs ({} still running)", done.len(), jobs.running_count())))
        }
        other => Err(super::unknown_action("job", other, "list, status, logs, wait, kill, clean")),
    }
}
