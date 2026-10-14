//! Minimal Chrome DevTools Protocol client: one websocket to the browser endpoint, flattened target
//! sessions (`sessionId` on each message), request/response matching and an event broadcast.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

/// Synthetic event sent once when the connection drops.
pub const CLOSED: &str = "nerve.closed";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct Cdp {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    pub events: broadcast::Sender<Arc<Event>>,
    alive: Arc<AtomicBool>,
}

impl Cdp {
    pub async fn connect(ws: &str) -> Result<Arc<Cdp>, String> {
        let mut cfg = WebSocketConfig::default();
        // full-page screenshots arrive as one big base64 frame
        cfg.max_message_size = Some(512 << 20);
        cfg.max_frame_size = Some(512 << 20);
        let fut = tokio_tungstenite::connect_async_with_config(ws, Some(cfg), false);
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .map_err(|_| format!("CDP connect {ws}: timed out"))?
            .map_err(|e| format!("CDP connect {ws}: {e}"))?;
        let (mut sink, mut read) = stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel::<Arc<Event>>(4096);
        let alive = Arc::new(AtomicBool::new(true));

        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(Message::Text(m.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (p2, ev2, al2) = (pending.clone(), events.clone(), alive.clone());
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                let txt = match msg {
                    Ok(Message::Text(t)) => t.to_string(),
                    Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(s) = p2.lock().unwrap().remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("CDP error").to_string()),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = s.send(r);
                    }
                } else if let Some(m) = v.get("method").and_then(Value::as_str) {
                    let _ = ev2.send(Arc::new(Event {
                        method: m.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                        session: v.get("sessionId").and_then(Value::as_str).map(String::from),
                    }));
                }
            }
            al2.store(false, Ordering::SeqCst);
            for (_, s) in p2.lock().unwrap().drain() {
                let _ = s.send(Err("browser connection closed".into()));
            }
            let _ = ev2.send(Arc::new(Event { method: CLOSED.into(), params: Value::Null, session: None }));
        });
        Ok(Arc::new(Cdp { tx, pending, next: AtomicU64::new(1), events, alive }))
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub async fn call(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if !self.alive() {
            return Err("browser connection closed".into());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (otx, orx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, otx);
        if self.tx.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("browser connection closed".into());
        }
        match tokio::time::timeout(timeout, orx).await {
            Ok(Ok(r)) => r.map_err(|e| format!("{method}: {e}")),
            Ok(Err(_)) => Err("browser connection closed".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method}: no answer in {}s (page hung?)", timeout.as_secs()))
            }
        }
    }
}
