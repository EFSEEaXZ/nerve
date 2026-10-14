#!/usr/bin/env python3
"""End-to-end smoke test for a running nerve server (stdlib only).

usage: python tests/test_smoke.py [base_url] [--token TOKEN]
default base_url: http://127.0.0.1:8100
"""
import base64
import json
import os
import sys
import tempfile
import time
import urllib.error
import urllib.request

BASE = next((a for a in sys.argv[1:] if a.startswith("http")), "http://127.0.0.1:8100").rstrip("/")
TOKEN = sys.argv[sys.argv.index("--token") + 1] if "--token" in sys.argv else None
SESSION = None
N = 0
FAILS = []


def post(msg, stream=True):
    global SESSION
    data = json.dumps(msg).encode()
    h = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream" if stream else "application/json"}
    if SESSION:
        h["Mcp-Session-Id"] = SESSION
    if TOKEN:
        h["Authorization"] = "Bearer " + TOKEN
    req = urllib.request.Request(BASE + "/mcp", data=data, headers=h, method="POST")
    with urllib.request.urlopen(req, timeout=120) as r:
        SESSION = r.headers.get("Mcp-Session-Id") or SESSION
        ct = r.headers.get("Content-Type", "")
        body = r.read().decode("utf-8", "replace")
    if r.status == 202 or not body.strip():
        return None, []
    if "text/event-stream" in ct:
        events = []
        for block in body.split("\n\n"):
            d = "\n".join(l[5:].lstrip() for l in block.splitlines() if l.startswith("data:"))
            if d:
                events.append(json.loads(d))
        final = [e for e in events if "id" in e and ("result" in e or "error" in e)]
        return (final[-1] if final else None), [e for e in events if e.get("method")]
    return json.loads(body), []


def rpc(method, params=None, stream=True):
    global N
    N += 1
    msg = {"jsonrpc": "2.0", "id": N, "method": method}
    if params is not None:
        msg["params"] = params
    return post(msg, stream)


def call(name, args, progress=False):
    params = {"name": name, "arguments": args}
    if progress:
        params["_meta"] = {"progressToken": "p%d" % (N + 1)}
    r, notes = rpc("tools/call", params)
    if r is None or "error" in r:
        raise RuntimeError("rpc error: %r" % (r,))
    res = r["result"]
    text = "\n".join(c.get("text", "") for c in res["content"] if c["type"] == "text")
    return res, text, notes


def check(label, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + label + ("" if cond else "  -> " + str(detail)[:600]))
    if not cond:
        FAILS.append(label)


def main():
    t0 = time.time()
    r, _ = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "1"}}, stream=False)
    check("initialize", r and r["result"]["serverInfo"]["name"] == "nerve", r)
    post({"jsonrpc": "2.0", "method": "notifications/initialized"})
    r, _ = rpc("tools/list", stream=False)
    names = [t["name"] for t in r["result"]["tools"]]
    check("tools/list", {"exec", "job", "fs", "image", "sys", "net"} <= set(names), names)
    for t in r["result"]["tools"]:
        s = t["inputSchema"]
        check("schema flat: " + t["name"], s.get("type") == "object" and "$defs" not in json.dumps(s), s)

    tmp = tempfile.mkdtemp(prefix="nerve-smoke-")
    win = os.name == "nt"
    # exec
    res, text, _ = call("exec", {"command": "echo hello-nerve"})
    check("exec echo", "hello-nerve" in text and not res["isError"], text)
    res, text, _ = call("exec", {"command": "exit 3"})
    check("exec exit code -> isError", res["isError"] and "3" in text, text)
    res, text, _ = call("exec", {"code": "import sys; print('py', sys.version_info[0]); print('привет')", "lang": "python"})
    check("exec python + unicode", "py 3" in text and "привет" in text, text)
    res, text, notes = call("exec", {"code": "import time\nfor i in range(4):\n    print('tick', i, flush=True); time.sleep(1)", "lang": "python"}, progress=True)
    check("exec progress notifications", len(notes) >= 1 and "tick 3" in text, (len(notes), text))
    res, text, _ = call("exec", {"code": "import time; time.sleep(30)", "lang": "python", "timeout_ms": 1500})
    check("exec timeout kills", "timed_out" in text or "timed out" in text, text)
    res, text, _ = call("exec", {"code": "print('x'*300000)", "lang": "python", "max_output": 5000})
    check("exec output clipped + saved", "omitted" in text or "saved" in text, text[:300])
    # background job
    res, text, _ = call("exec", {"code": "import time\nprint('ready', flush=True)\ntime.sleep(2)\nprint('done', flush=True)", "lang": "python", "background": True, "label": "smoke"})
    jid = (res.get("structuredContent") or {}).get("job_id") or text.split()[2]
    check("exec background", text.startswith("started job"), text)
    res, text, _ = call("job", {"action": "wait", "id": jid, "until": "ready", "timeout_ms": 15000})
    check("job wait until", "matched" in text, text)
    res, text, _ = call("job", {"action": "wait", "id": jid, "timeout_ms": 20000})
    check("job wait finish", "exited(0)" in text and "done" in text, text)
    res, text, _ = call("job", {"action": "logs", "id": jid, "stream": "stdout", "offset": 0})
    check("job logs offset", "ready" in text and "next_offset" in text, text)
    res, text, _ = call("job", {"action": "list"})
    check("job list", jid in text, text)
    res, text, _ = call("job", {"action": "clean", "id": jid})
    check("job clean", "removed" in text, text)

    # fs
    f = os.path.join(tmp, "a", "test.txt")
    res, text, _ = call("fs", {"action": "write", "path": f, "content": "line1\nalpha beta\nline3\n"})
    check("fs write (mkdir parents)", "created" in text, text)
    res, text, _ = call("fs", {"action": "read", "path": f, "numbered": True})
    check("fs read numbered", "2\talpha beta" in text, text)
    res, text, _ = call("fs", {"action": "edit", "path": f, "old": "alpha beta", "new": "gamma"})
    check("fs edit", "edited" in text and "gamma" in text, text)
    res, text, _ = call("fs", {"action": "edit", "path": f, "old": "nope", "new": "x"})
    check("fs edit not found -> error", res["isError"], text)
    crlf = os.path.join(tmp, "crlf.txt")
    with open(crlf, "wb") as h:
        h.write("один\r\nдва\r\nтри\r\n".encode("cp1251"))
    res, text, _ = call("fs", {"action": "read", "path": crlf})
    check("fs read cp1251+CRLF", "два" in text and "windows-1251" in text, text)
    res, text, _ = call("fs", {"action": "edit", "path": crlf, "old": "два\nтри", "new": "2\n3"})
    raw = open(crlf, "rb").read()
    check("fs edit keeps CRLF + encoding", raw == "один\r\n2\r\n3\r\n".encode("cp1251"), (text, raw))
    u16 = os.path.join(tmp, "u16.txt")
    with open(u16, "wb") as h:
        h.write("\ufeffunicode текст\r\n".encode("utf-16-le"))
    res, text, _ = call("fs", {"action": "read", "path": u16})
    check("fs read utf-16", "unicode текст" in text, text)
    res, text, _ = call("fs", {"action": "list", "path": tmp, "depth": 3})
    check("fs list", "test.txt" in text, text)
    res, text, _ = call("fs", {"action": "find", "path": tmp, "pattern": "*.txt"})
    check("fs find glob", "test.txt" in text and "crlf.txt" in text, text)
    res, text, _ = call("fs", {"action": "grep", "path": tmp, "pattern": "gam+a", "context": 1})
    check("fs grep", "gamma" in text and "1 match" in text, text)
    res, text, _ = call("fs", {"action": "copy", "path": os.path.join(tmp, "a"), "dest": os.path.join(tmp, "b")})
    check("fs copy dir", "copied" in text, text)
    res, text, _ = call("fs", {"action": "hash", "path": f})
    check("fs hash", len(text.split()[0]) == 64, text)
    res, text, _ = call("fs", {"action": "size", "path": tmp})
    check("fs size", "files" in text, text)
    res, text, _ = call("fs", {"action": "remove", "path": "C:\\" if win else "/"})
    check("fs remove root refused", res["isError"] and "refused" in text, text)
    res, text, _ = call("fs", {"action": "remove", "path": os.path.join(tmp, "b"), "recursive": True})
    check("fs remove dir", "removed" in text, text)
    res, text, _ = call("fs", {"action": "read", "path": f, "offset": -1})
    check("fs tail", "line3" in text, text)

    # image: generate a PNG with python stdlib (zlib) to avoid dependencies
    import struct
    import zlib
    w, h = 1200, 800
    rows = b"".join(b"\x00" + bytes(v for x in range(w) for v in ((x * 7) % 256, (y * 3) % 256, ((x * y) >> 4) % 256)) for y in range(h))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b"")
    imgp = os.path.join(tmp, "noise.png")
    open(imgp, "wb").write(png)
    res, text, _ = call("image", {"path": imgp, "max_bytes": 30000})
    imgs = [c for c in res["content"] if c["type"] == "image"]
    size = len(base64.b64decode(imgs[0]["data"])) if imgs else 0
    check("image view within budget", imgs and size <= 30000, (text, size))
    res, text, _ = call("image", {"path": imgp, "tile": "2x2"})
    check("image tiles", len([c for c in res["content"] if c["type"] == "image"]) == 4, text)
    res, text, _ = call("image", {"path": imgp, "region": [0.5, 0.5, 0.5, 0.5], "max_bytes": 60000})
    check("image region fractions", "region 600,400 600x400" in text, text)
    res, text, _ = call("image", {"action": "info", "path": imgp})
    check("image info", "1200x800" in text, text)

    # sys / net
    res, text, _ = call("sys", {"action": "info"})
    check("sys info", "cpu:" in text and "ram:" in text, text)
    res, text, _ = call("sys", {"action": "ps", "filter": "nerve"})
    check("sys ps", "nerve" in text, text)
    res, text, _ = call("sys", {"action": "status"})
    check("sys status", "nerve" in text and "sessions" in text, text)
    res, text, _ = call("sys", {"action": "log", "limit": 5})
    check("sys log", not res["isError"], text)
    res, text, _ = call("net", {"action": "http", "url": BASE + "/health"})
    check("net http", "HTTP 200" in text, text)
    res, text, _ = call("net", {"action": "tcp", "host": "127.0.0.1", "port": int(BASE.rsplit(":", 1)[1].split("/")[0])})
    check("net tcp", "open" in text, text)

    # protocol edge cases
    r, _ = rpc("tools/call", {"name": "nope", "arguments": {}})
    check("unknown tool -> error", r is not None and ("error" in r or r["result"].get("isError")), r)
    res, text, _ = call("fs", {"action": "read"})
    check("missing param -> isError", res["isError"] and "path" in text, text)
    r, _ = rpc("ping", stream=False)
    check("ping", r and "result" in r, r)

    print("\n%d checks failed" % len(FAILS) if FAILS else "\nALL PASSED", "in %.1fs" % (time.time() - t0))
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()