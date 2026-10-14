#!/usr/bin/env python3
"""Phase-2 lifecycle test on an isolated nerve instance (own data dir + ports 8300/8301).

Covers: dashboard + guards, control key, tunnel (command or zrok) online + adoption across
restart and hot update, unsupervised successor restart, job survival, MCP session continuity,
tunnel restart, graceful stop with tunnel cleanup.

usage: python tests/test_service.py <path to nerve(.exe)> [--zrok RESERVED_NAME]
"""
import ctypes
import http.client
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

EXE = sys.argv[1]
ZROK = sys.argv[sys.argv.index("--zrok") + 1] if "--zrok" in sys.argv else None
WIN = os.name == "nt"
HOME = os.path.join(tempfile.gettempdir(), "nerve-p2test")
LOCAL, PUBLIC = "127.0.0.1:8300", "127.0.0.1:8301"
results = []
SID = None
N = 0


def check(label, cond, detail=""):
    results.append((label, bool(cond)))
    print(("PASS " if cond else "FAIL ") + label + (f"  [{detail}]" if detail and not cond else ""), flush=True)
    return cond


def req(method, path, body=None, headers=None, host=LOCAL, timeout=15, https=False):
    h, p = host.split(":") if ":" in host else (host, 443)
    c = http.client.HTTPSConnection(h, int(p), timeout=timeout) if https else http.client.HTTPConnection(h, int(p), timeout=timeout)
    hd = {"Host": host if not https else h}
    if body is not None and not isinstance(body, (bytes, str)):
        body = json.dumps(body)
        hd["Content-Type"] = "application/json"
    hd.update(headers or {})
    c.request(method, path, body=body, headers=hd)
    r = c.getresponse()
    data = r.read().decode("utf-8", "replace")
    return r.status, dict((k.lower(), v) for k, v in r.getheaders()), data


def health():
    try:
        s, _, b = req("GET", "/health", timeout=3)
        return json.loads(b) if s == 200 else None
    except Exception:
        return None


def state():
    return json.loads(req("GET", "/api/state")[2])


def mcp(method, params=None, init=False):
    global SID, N
    N += 1
    msg = {"jsonrpc": "2.0", "id": N, "method": method, "params": params or {}}
    hd = {"Accept": "application/json, text/event-stream"}
    if SID and not init:
        hd["mcp-session-id"] = SID
    s, h, b = req("POST", "/mcp", msg, hd, timeout=120)
    if "mcp-session-id" in h:
        SID = h["mcp-session-id"]
    if "event-stream" in h.get("content-type", ""):
        for line in b.splitlines():
            if line.startswith("data:"):
                v = json.loads(line[5:].strip())
                if v.get("id") == N:
                    return v
        raise RuntimeError("no response in SSE: " + b[:300])
    return json.loads(b)


def call(name, args):
    v = mcp("tools/call", {"name": name, "arguments": args})
    r = v.get("result", {})
    return "".join(c.get("text", "") for c in r.get("content", []) if c.get("type") == "text"), r.get("isError")


def alive(pid):
    if not pid:
        return False
    if WIN:
        k = ctypes.windll.kernel32
        h = k.OpenProcess(0x1000, False, pid)
        if not h:
            return False
        code = ctypes.c_ulong()
        k.GetExitCodeProcess(h, ctypes.byref(code))
        k.CloseHandle(h)
        return code.value == 259
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def wait(fn, timeout, step=0.25):
    end = time.time() + timeout
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(step)
    return None


def tunnel():
    try:
        return state()["status"]["tunnel"]
    except Exception:
        return {}


def main():
    shutil.rmtree(HOME, ignore_errors=True)
    os.makedirs(os.path.join(HOME, "bin"))
    exe = os.path.join(HOME, "bin", os.path.basename(EXE))
    shutil.copy2(EXE, exe)
    if ZROK:
        tun = f'provider = "zrok"\nzrok_name = "{ZROK}"\n'
    else:
        py = sys.executable
        cmd = f'"{py}" -u -c "import time;print(\'https://p2test.example.com\',flush=True);time.sleep(9999)"'
        tun = f"provider = \"command\"\ncommand = '''{cmd}'''\n"
    with open(os.path.join(HOME, "config.toml"), "w") as f:
        f.write(f'[server]\nlocal_addr = "{LOCAL}"\npublic_addr = "{PUBLIC}"\n\n[tunnel]\n{tun}\n[ui]\ntray = false\n')
    log = open(os.path.join(HOME, "first.out"), "w")
    flags = 0x00000200 | 0x08000000 if WIN else 0
    subprocess.Popen([exe, "--home", HOME, "serve"], stdout=log, stderr=log, stdin=subprocess.DEVNULL, creationflags=flags)

    h = wait(health, 20)
    if not check("server up", h):
        return
    pid1 = h["pid"]
    s, _, b = req("GET", "/ui")
    check("dashboard /ui", s == 200 and "<title>nerve" in b, s)
    s, hd, _ = req("GET", "/", headers={"Accept": "text/html"})
    check("browser / -> /ui", s in (302, 303, 307) and hd.get("location") == "/ui", (s, hd.get("location")))
    st = state()
    check("api/state", st["status"]["version"] == "0.2.0" and "jobs" in st, st["status"].get("version"))
    s, _, _ = req("POST", "/api/action", {"action": "tunnel_restart"}, {"Origin": "https://evil.example"})
    check("foreign Origin rejected", s == 403, s)
    s, _, _ = req("POST", "/_nerve/control", {"action": "ping"})
    check("control without key rejected", s == 403, s)
    s, _, _ = req("GET", "/ui", host=PUBLIC)
    check("dashboard not on public listener", s in (401, 404), s)

    t = wait(lambda: (lambda x: x if x.get("state") == "online" else None)(tunnel()), 40)
    check("tunnel online", t, tunnel())
    t1 = (t or {}).get("pid")
    if ZROK and t:
        try:
            s, _, b = req("GET", "/health", host=f"{ZROK}.shares.zrok.io", https=True, timeout=20)
            check("public health via zrok", s == 200 and '"nerve"' in b, (s, b[:120]))
        except Exception as e:
            check("public health via zrok", False, e)

    mcp("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "p2test", "version": "1"}}, init=True)
    code = "import time\nfor i in range(30):\n    print('tick', i, flush=True)\n    time.sleep(1)\nprint('survived', flush=True)"
    text, _ = call("exec", {"code": code, "lang": "python", "background": True, "label": "p2"})
    job = text.split()[2]
    check("job started", job.startswith("j"), text[:80])

    text, err = call("sys", {"action": "restart"})
    check("sys restart accepted", not err and "restarting" in text, text[:120])
    h = wait(lambda: (lambda x: x if x and x["pid"] != pid1 else None)(health()), 30)
    check("successor took over", h, h)
    pid2 = h and h["pid"]
    check("old instance exited", wait(lambda: not alive(pid1), 10))
    t = wait(lambda: (lambda x: x if x.get("state") == "online" else None)(tunnel()), 20) or {}
    check("tunnel adopted after restart", t.get("pid") == t1 and t.get("adopted"), t)
    text, err = call("sys", {"action": "status"})
    check("MCP session survives restart", not err and "nerve 0.2.0" in text, text[:100])

    new = os.path.join(HOME, "nerve-next" + (".exe" if WIN else ""))
    shutil.copy2(EXE, new)
    text, err = call("sys", {"action": "update", "source": new})
    check("sys update accepted", not err and "installed" in text, text[:160])
    h = wait(lambda: (lambda x: x if x and x["pid"] != pid2 else None)(health()), 30)
    check("updated instance up", h, h)
    pid3 = h and h["pid"]
    olds = [f for f in os.listdir(os.path.join(HOME, "bin")) if f.startswith("nerve.old-")]
    check("binary swapped (old cleaned up or kept)", os.path.exists(exe), olds)
    t = wait(lambda: (lambda x: x if x.get("state") == "online" else None)(tunnel()), 20) or {}
    check("tunnel adopted after update", t.get("pid") == t1, t)

    text, _ = call("job", {"action": "wait", "id": job, "timeout_ms": 45000})
    check("job survived restart + update", "survived" in text and "exited(0)" in text, text[-200:])

    s, _, b = req("POST", "/api/action", {"action": "tunnel_restart"})
    t = wait(lambda: (lambda x: x if x.get("state") == "online" and x.get("pid") not in (None, t1) else None)(tunnel()), 40) or {}
    check("tunnel restart", t.get("pid") and t.get("pid") != t1 and not alive(t1), t)
    t2 = t.get("pid")

    key = open(os.path.join(HOME, "control.key")).read().strip()
    s, _, _ = req("POST", "/_nerve/control", {"action": "shutdown", "keep_tunnel": False}, {"x-nerve-key": key})
    check("control shutdown", s == 200, s)
    check("server exited", wait(lambda: not alive(pid3), 20))
    check("tunnel stopped with server", wait(lambda: not alive(t2), 10))
    check("tunnel.json removed", not os.path.exists(os.path.join(HOME, "tunnel.json")))


try:
    main()
finally:
    ok = sum(1 for _, c in results if c)
    print(f"\n{ok}/{len(results)} passed")
    sys.exit(0 if results and ok == len(results) else 1)
