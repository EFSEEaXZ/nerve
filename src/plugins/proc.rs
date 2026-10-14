//! One running plugin process: JSON-RPC over its stdin/stdout, stderr into the plugin log.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::Log;
use crate::mcp::{Progress, ToolCtx};
use crate::server::App;

pub struct Proc {
    pub pid: u32,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    progress: Arc<Mutex<HashMap<String, Progress>>>,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    child: Mutex<Option<Child>>,
}

impl Proc {
    pub fn spawn(program: &str, args: &[String], dir: &Path, env: &[(String, String)], log: Arc<Log>, app: Option<Weak<App>>) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let progress: Arc<Mutex<HashMap<String, Progress>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

        // stderr -> log
        {
            let log = log.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    log.line(&l);
                }
            });
        }
        // stdout -> responses / notifications / requests from the plugin
        {
            let (pending, progress, alive, log) = (pending.clone(), progress.clone(), alive.clone(), log.clone());
            tokio::spawn(async move {
                let mut r = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match r.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(line) else {
                        log.line(&format!("[stdout] {line}"));
                        continue;
                    };
                    let method = msg.get("method").and_then(|m| m.as_str()).map(String::from);
                    let id = msg.get("id").filter(|i| !i.is_null()).cloned();
                    match (method, id) {
                        (None, Some(id)) => {
                            if let Some(n) = id.as_u64() {
                                if let Some(tx) = pending.lock().unwrap().remove(&n) {
                                    let _ = tx.send(msg);
                                }
                            }
                        }
                        (Some(m), None) => match m.as_str() {
                            "notifications/progress" => {
                                let p = &msg["params"];
                                let tok = match &p["progressToken"] {
                                    Value::String(s) => s.clone(),
                                    o => o.to_string(),
                                };
                                if let Some(pr) = progress.lock().unwrap().get(&tok) {
                                    pr.send_value(
                                        p["progress"].as_f64().unwrap_or(0.0),
                                        p["total"].as_f64(),
                                        p["message"].as_str().unwrap_or("").to_string(),
                                    );
                                }
                            }
                            "notifications/message" => {
                                let p = &msg["params"];
                                let data = match &p["data"] {
                                    Value::String(s) => s.clone(),
                                    o => o.to_string(),
                                };
                                log.line(&format!("[{}] {data}", p["level"].as_str().unwrap_or("info")));
                            }
                            _ => {}
                        },
                        (Some(m), Some(id)) => {
                            let reply_tx = reply_tx.clone();
                            let app = app.clone();
                            let params = msg.get("params").cloned().unwrap_or(Value::Null);
                            tokio::spawn(async move {
                                let resp = handle_plugin_request(&m, params, app).await;
                                let mut out = json!({"jsonrpc": "2.0", "id": id});
                                match resp {
                                    Ok(v) => out["result"] = v,
                                    Err((code, e)) => out["error"] = json!({"code": code, "message": e}),
                                }
                                let _ = reply_tx.send(out);
                            });
                        }
                        (None, None) => {}
                    }
                }
                alive.store(false, Ordering::SeqCst);
                pending.lock().unwrap().clear(); // wakes waiters with an error
                log.line("--- process exited");
            });
        }
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        // replies to plugin->nerve requests are written by a separate task (needs the stdin lock)
        let stdin_ref = Arc::downgrade(&stdin);
        let p = Self { pid, stdin, pending, progress, next: AtomicU64::new(1), alive, child: Mutex::new(Some(child)) };
        tokio::spawn(async move {
            while let Some(v) = reply_rx.recv().await {
                let Some(s) = stdin_ref.upgrade() else { break };
                let mut line = v.to_string();
                line.push('\n');
                let mut w = s.lock().await;
                if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
        });
        Ok(p)
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn write(&self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        let mut w = self.stdin.lock().await;
        w.write_all(line.as_bytes()).await.map_err(|e| format!("write to plugin: {e}"))?;
        w.flush().await.map_err(|e| format!("write to plugin: {e}"))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Option<Duration>, cancel: Option<&CancellationToken>) -> Result<Value, String> {
        if !self.alive() {
            return Err("plugin process is not running".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);
        let sleep = async {
            match timeout {
                Some(t) => tokio::time::sleep(t).await,
                None => std::future::pending::<()>().await,
            }
        };
        let msg = tokio::select! {
            r = rx => r.map_err(|_| "plugin process exited during the call".to_string())?,
            _ = sleep => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.notify("notifications/cancelled", json!({"requestId": id, "reason": "timeout"})).await;
                return Err(format!("timed out after {} ms", timeout.map(|t| t.as_millis()).unwrap_or(0)));
            }
            _ = cancel.cancelled() => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.notify("notifications/cancelled", json!({"requestId": id, "reason": "cancelled by client"})).await;
                return Err("cancelled".into());
            }
        };
        if let Some(e) = msg.get("error") {
            return Err(e.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| e.to_string()));
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn register_progress(&self, p: Progress) -> String {
        let tok = format!("p{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.progress.lock().unwrap().insert(tok.clone(), p);
        tok
    }

    pub fn unregister_progress(&self, tok: &str) {
        self.progress.lock().unwrap().remove(tok);
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            // only the plugin itself: apps it launched (Blender, Unreal...) must survive a reload
            let _ = c.start_kill();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn handle_plugin_request(method: &str, params: Value, app: Option<Weak<App>>) -> Result<Value, (i64, String)> {
    match method {
        "ping" => Ok(json!({})),
        // call one of nerve's built-in tools: {name, arguments}
        "nerve/call" | "tools/call" => {
            let app = app.and_then(|w| w.upgrade()).ok_or((-32603, "nerve is shutting down".to_string()))?;
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let ctx = ToolCtx { app: app.clone(), progress: Progress::new(None, None), cancel: CancellationToken::new() };
            match app.registry.call(&name, args, ctx).await {
                Some(o) => Ok(o.into_result(true)),
                None => Err((-32602, format!("unknown built-in tool `{name}`"))),
            }
        }
        "nerve/info" => {
            let app = app.and_then(|w| w.upgrade()).ok_or((-32603, "nerve is shutting down".to_string()))?;
            Ok(crate::server::status_json(&app))
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}
