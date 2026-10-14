//! `net`: HTTP requests/downloads and TCP/DNS checks.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, need, tool};
use crate::util::{self, fmt_bytes, fmt_dur_ms};

#[derive(Deserialize, JsonSchema)]
pub struct NetArgs {
    /// http | download | tcp | dns
    action: String,
    /// http/download: URL.
    url: Option<String>,
    /// http: GET (default) | POST | PUT | PATCH | DELETE | HEAD | OPTIONS
    method: Option<String>,
    /// http: request headers.
    headers: Option<HashMap<String, String>>,
    /// http: raw request body.
    body: Option<String>,
    /// http: JSON request body (sets Content-Type).
    json: Option<Value>,
    /// http: stream the response body to this file instead of returning it (any size, with progress).
    save_to: Option<String>,
    /// http: max response bytes returned inline (default 100000).
    max_bytes: Option<usize>,
    /// Timeout in ms (http default 120000, 0 = none; tcp default 5000).
    timeout_ms: Option<u64>,
    /// http: accept invalid TLS certificates.
    insecure: Option<bool>,
    /// http: include all response headers.
    verbose: Option<bool>,
    /// tcp/dns: host name or IP.
    host: Option<String>,
    /// tcp: port.
    port: Option<u16>,
}

const DESC: &str = "Network. Actions: http {url, method?, headers?, body?|json?, save_to?, timeout_ms?, verbose?} \
(text bodies inline up to max_bytes; save_to streams any size to disk with progress; binary bodies need save_to); \
download {url, save_to} (same as http GET + save_to); tcp {host, port} (connect check + latency); dns {host}.";

pub fn def() -> ToolDef {
    tool::<NetArgs, _, _>("net", "Network", DESC, json!({"destructiveHint": false, "openWorldHint": true}), run)
}

async fn run(a: NetArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    match a.action.as_str() {
        "http" | "request" | "fetch" | "get" | "post" => http(a, ctx).await,
        "download" => {
            need(a.save_to.as_deref(), "save_to")?;
            http(a, ctx).await
        }
        "tcp" | "port" | "connect" => tcp(a).await,
        "dns" | "resolve" => dns(a).await,
        other => Err(super::unknown_action("net", other, "http, download, tcp, dns")),
    }
}

fn is_texty(ct: &str) -> bool {
    let ct = ct.to_ascii_lowercase();
    ct.is_empty()
        || ct.starts_with("text/")
        || ["json", "xml", "javascript", "yaml", "toml", "csv", "x-www-form-urlencoded", "html", "graphql", "event-stream"]
            .iter()
            .any(|k| ct.contains(k))
}

async fn http(a: NetArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let url = need(a.url.as_deref(), "url")?;
    let url = if url.contains("://") { url.to_string() } else { format!("https://{url}") };
    let method = a.method.as_deref().unwrap_or(if a.action == "post" { "POST" } else { "GET" }).to_ascii_uppercase();
    let method = reqwest::Method::from_bytes(method.as_bytes()).ctx("bad method")?;
    let client = if a.insecure.unwrap_or(false) {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .user_agent(concat!("nerve/", env!("CARGO_PKG_VERSION")))
            .build()
            .ctx("http client")?
    } else {
        ctx.app.http.clone()
    };
    let mut req = client.request(method.clone(), &url);
    let tms = a.timeout_ms.unwrap_or(120_000);
    if tms > 0 {
        req = req.timeout(Duration::from_millis(tms));
    }
    for (k, v) in a.headers.iter().flatten() {
        req = req.header(k, v);
    }
    if let Some(j) = &a.json {
        req = req.json(j);
    } else if let Some(b) = &a.body {
        req = req.body(b.clone());
    }
    let t0 = Instant::now();
    let resp = tokio::select! {
        r = req.send() => r.map_err(|e| format!("request failed: {e}{}", source_chain(&e)))?,
        _ = ctx.cancel.cancelled() => return Err("cancelled".into()),
    };
    let status = resp.status();
    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();
    let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let len = resp.content_length();
    let mut head = format!("HTTP {} · {method} {final_url}\n", status);
    let wanted = ["content-type", "content-length", "location", "content-disposition", "server", "www-authenticate", "retry-after"];
    for (k, v) in headers.iter() {
        if a.verbose.unwrap_or(false) || wanted.contains(&k.as_str()) {
            head.push_str(&format!("{k}: {}\n", v.to_str().unwrap_or("<binary>")));
        }
    }
    let mut hj = serde_json::Map::new();
    for (k, v) in headers.iter() {
        hj.insert(k.to_string(), json!(v.to_str().unwrap_or("")));
    }
    let base = json!({"status": status.as_u16(), "url": final_url, "headers": hj});
    if let Some(path) = &a.save_to {
        let p = util::resolve_path(path, Some(&ctx.app.default_dir.to_string_lossy()));
        if let Some(d) = p.parent() {
            tokio::fs::create_dir_all(d).await.ctx(format!("create {}", d.display()))?;
        }
        let tmp = p.with_extension("part");
        let mut f = tokio::fs::File::create(&tmp).await.ctx(format!("create {}", tmp.display()))?;
        let mut stream = resp.bytes_stream();
        let mut n: u64 = 0;
        let mut last = Instant::now();
        loop {
            let chunk = tokio::select! {
                c = stream.next() => c,
                _ = ctx.cancel.cancelled() => {
                    drop(f);
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err("download cancelled".into());
                }
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|e| format!("download interrupted after {}: {e}", fmt_bytes(n)))?;
            f.write_all(&chunk).await.ctx("write")?;
            n += chunk.len() as u64;
            if last.elapsed() >= Duration::from_secs(1) {
                last = Instant::now();
                let total = len.map(|l| l as f64);
                ctx.progress.send_value(n as f64, total, format!("{} of {}", fmt_bytes(n), len.map(fmt_bytes).unwrap_or_else(|| "?".into())));
            }
        }
        f.flush().await.ctx("flush")?;
        drop(f);
        let _ = tokio::fs::remove_file(&p).await;
        tokio::fs::rename(&tmp, &p).await.ctx(format!("rename to {}", p.display()))?;
        let dur = t0.elapsed().as_millis() as u64;
        let rate = if dur > 0 { format!(", {}/s", fmt_bytes(n * 1000 / dur)) } else { String::new() };
        head.push_str(&format!("saved {} to {} in {}{rate}", fmt_bytes(n), p.display(), fmt_dur_ms(dur)));
        let mut st = base;
        st["saved_to"] = json!(p);
        st["bytes"] = json!(n);
        return Ok(ToolOutput::text(head).with_structured(st).error_if(!status.is_success()));
    }
    if method == reqwest::Method::HEAD {
        return Ok(ToolOutput::text(head.trim_end().to_string()).with_structured(base).error_if(!status.is_success() && !status.is_redirection()));
    }
    let max = a.max_bytes.unwrap_or(100_000).max(256);
    let mut buf: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = tokio::select! {
        c = stream.next() => c,
        _ = ctx.cancel.cancelled() => return Err("cancelled".into()),
    } {
        let chunk = chunk.map_err(|e| format!("reading body failed: {e}"))?;
        total += chunk.len() as u64;
        if buf.len() < max {
            let take = (max - buf.len()).min(chunk.len());
            buf.extend_from_slice(&chunk[..take]);
        }
        if total > (max as u64) * 4 && total > 8 << 20 {
            break; // don't download huge bodies just to discard them
        }
    }
    let dur = t0.elapsed().as_millis() as u64;
    head.push_str(&format!("{} in {}\n\n", fmt_bytes(total), fmt_dur_ms(dur)));
    if !is_texty(&ct) && util::looks_binary(&buf) {
        head.push_str(&format!("<binary body ({ct}); use save_to to store it>"));
    } else {
        let text = String::from_utf8_lossy(&buf);
        let text = if ct.contains("json") {
            serde_json::from_slice::<Value>(&buf).ok().and_then(|v| serde_json::to_string_pretty(&v).ok()).map(Into::into).unwrap_or(text)
        } else {
            text
        };
        head.push_str(&text);
        if total as usize > buf.len() {
            head.push_str(&format!("\n…[truncated: showing {} of {}; raise max_bytes or use save_to]", fmt_bytes(buf.len() as u64), fmt_bytes(total)));
        }
    }
    let mut st = base;
    st["bytes"] = json!(total);
    Ok(ToolOutput::text(head).with_structured(st).error_if(!status.is_success() && !status.is_redirection()))
}

fn source_chain(e: &dyn std::error::Error) -> String {
    let mut s = String::new();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(&format!(": {c}"));
        cur = c.source();
    }
    s
}

async fn tcp(a: NetArgs) -> Result<ToolOutput, String> {
    let host = need(a.host.as_deref(), "host")?.to_string();
    let port = need(a.port, "port")?;
    let tms = a.timeout_ms.unwrap_or(5000).max(100);
    let t0 = Instant::now();
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.ctx(format!("resolve {host}"))?.collect();
    let r = tokio::time::timeout(Duration::from_millis(tms), tokio::net::TcpStream::connect(&addrs[..])).await;
    let dur = t0.elapsed().as_millis() as u64;
    let ips: Vec<String> = addrs.iter().map(|a| a.ip().to_string()).collect();
    Ok(match r {
        Ok(Ok(s)) => ToolOutput::text(format!(
            "{host}:{port} open · connected to {} in {}",
            s.peer_addr().map(|a| a.to_string()).unwrap_or_default(),
            fmt_dur_ms(dur)
        )),
        Ok(Err(e)) => ToolOutput::error(format!("{host}:{port} closed/refused ({e}) · tried {}", ips.join(", "))),
        Err(_) => ToolOutput::error(format!("{host}:{port} timed out after {} · tried {}", fmt_dur_ms(tms), ips.join(", "))),
    })
}

async fn dns(a: NetArgs) -> Result<ToolOutput, String> {
    let host = need(a.host.as_deref().or(a.url.as_deref()), "host")?;
    let host = url::Url::parse(host).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| host.to_string());
    let t0 = Instant::now();
    let addrs: Vec<String> = tokio::net::lookup_host((host.as_str(), 0)).await.ctx(format!("resolve {host}"))?.map(|a| a.ip().to_string()).collect();
    let mut uniq: Vec<String> = vec![];
    for a in addrs {
        if !uniq.contains(&a) {
            uniq.push(a);
        }
    }
    Ok(ToolOutput::text(format!("{host} → {} ({})", uniq.join(", "), fmt_dur_ms(t0.elapsed().as_millis() as u64))))
}