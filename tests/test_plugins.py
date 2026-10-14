"""Plugins test (hot reload, blender, unreal) against an isolated nerve instance.

usage: python tests/test_plugins.py [path\\to\\nerve.exe] [--no-blender]
Starts nerve with its own data dir on 127.0.0.1:8500 (no tunnel/tray). Blender runs headless only.
"""

import json, os, struct, subprocess, sys, tempfile, threading, time, urllib.request, shutil, zlib

EXE = next((a for a in sys.argv[1:] if not a.startswith("--")), r"target\release\nerve.exe")
BLENDER = "--no-blender" not in sys.argv
PORT = 8500
URL = f"http://127.0.0.1:{PORT}/mcp"
HOME = os.path.join(tempfile.gettempdir(), "nerve-p5")
PL = os.path.join(HOME, "plugins")
ok = fail = 0
sid = None
rid = 0


def check(name, cond, extra=""):
    global ok, fail
    if cond:
        ok += 1; print(f"  PASS {name}")
    else:
        fail += 1; print(f"  FAIL {name} :: {str(extra)[:900]}")


_lock = threading.Lock()


def rpc(method, params=None, timeout=300):
    global sid, rid
    with _lock:
        rid += 1
        my = rid
    body = json.dumps({"jsonrpc": "2.0", "id": my, "method": method, "params": params or {}}).encode()
    h = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if sid:
        h["Mcp-Session-Id"] = sid
    with urllib.request.urlopen(urllib.request.Request(URL, data=body, headers=h, method="POST"), timeout=timeout) as r:
        sid = r.headers.get("Mcp-Session-Id") or sid
        raw = r.read().decode("utf-8", "replace")
    msgs = [json.loads(raw)] if raw.lstrip().startswith("{") else [json.loads(l[5:]) for l in raw.splitlines() if l.startswith("data:") and l[5:].strip()]
    for m in msgs:
        if m.get("id") == my:
            m["_all"] = msgs
            return m
    raise RuntimeError(raw[:300])


def call(_tool, _timeout=300, _meta=None, **args):
    params = {"name": _tool, "arguments": args}
    if _meta:
        params["_meta"] = _meta
    m = rpc("tools/call", params, _timeout)
    if "error" in m:
        return True, "RPC ERROR: " + m["error"].get("message", ""), [], m
    res = m.get("result", {})
    text = "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
    return res.get("isError", False), text, [c for c in res.get("content", []) if c.get("type") == "image"], m


def tools():
    return {t["name"]: t for t in rpc("tools/list")["result"]["tools"]}


def wait_for(pred, secs=8):
    t0 = time.time()
    while time.time() - t0 < secs:
        try:
            v = pred()
            if v:
                return v
        except Exception:
            pass
        time.sleep(0.25)
    return None


def start_server():
    p = subprocess.Popen([EXE, "--home", HOME, "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(150):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=1); break
        except Exception:
            time.sleep(0.2)
    global sid
    sid = None
    rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "p5", "version": "1"}})
    return p


def stop_server(p):
    p.terminate()
    try:
        p.wait(10)
    except Exception:
        p.kill()


def png(path, w=64, h=48):
    rows = b"".join(b"\x00" + b"".join(bytes([(x * 4) % 256, (y * 5) % 256, 128]) for x in range(w)) for y in range(h))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
    open(path, "wb").write(data)


DEMO_V2 = r'''
import os, time
from nerve_plugin import Plugin, Image, ToolError
from typing import Literal, Optional

p = Plugin("demo", version="0.2.0")

@p.tool
def add(a: int, b: int = 0) -> str:
    """Add numbers.

    Args:
        a: first
        b: second
    """
    return str(a + b)

@p.tool
def pick(mode: Literal["x", "y"], tags: Optional[list] = None):
    """Pick."""
    return {"mode": mode, "tags": tags or []}

@p.tool
def slow(ctx, steps: int = 5):
    """Slow with progress."""
    for i in range(steps):
        ctx.check()
        ctx.progress(f"step {i}", i + 1, steps)
        time.sleep(0.2)
    return "done"

@p.tool
def img(path: str):
    """Image by path."""
    return [Image(path), "an image"]

@p.tool
def bad():
    """Clean error."""
    raise ToolError("nope, on purpose")

@p.tool
def boom():
    """Exception."""
    return 1 / 0

@p.tool
def crash():
    """Kill the process."""
    os._exit(3)

@p.tool
def callback(ctx, path: str):
    """Call nerve's fs tool."""
    return ctx.nerve_text("fs", action="stat", path=path)

@p.tool
def noisy():
    """print() must not break the protocol."""
    print("hello from print")
    return "quiet"

if __name__ == "__main__":
    p.run()
'''


def main():
    shutil.rmtree(HOME, ignore_errors=True)
    os.makedirs(HOME, exist_ok=True)
    with open(os.path.join(HOME, "config.toml"), "w") as f:
        f.write(f'[server]\nlocal_addr = "127.0.0.1:{PORT}"\npublic_addr = ""\n[tunnel]\nprovider = "none"\n[ui]\ntray = false\ndashboard = false\n[browser]\nheadless = true\n')
    srv = start_server()
    try:
        print("== bundled + registry")
        for rel in ["README.md", "_sdk/nerve_plugin.py", "blender/server.py", "blender/nerve_bridge.py", "unreal/server.py", ".bundled.json"]:
            check(f"bundled {rel}", os.path.exists(os.path.join(PL, rel)))
        T = tools()
        check("plugin tool", "plugin" in T, list(T))
        check("blender tool exposed", "blender" in T and T["blender"]["description"].startswith("[plugin blender]"), list(T))
        check("unreal tool exposed", "unreal" in T, list(T))
        check("blender schema has action enum", "launch" in T["blender"]["inputSchema"]["properties"]["action"]["enum"], T["blender"]["inputSchema"])
        e, t, _, _ = call("plugin", action="list")
        check("plugin list", not e and "blender" in t and "[running]" in t, t)
        h = json.load(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health"))
        check("health lists plugin tools", "blender" in h["tools"] and any(p["name"] == "unreal" for p in h["plugins"]), h.get("tools"))

        print("== scaffold + hot reload")
        # listen for list_changed on the GET stream
        seen = {"n": 0}
        def listen():
            try:
                req = urllib.request.Request(URL, headers={"Accept": "text/event-stream", "Mcp-Session-Id": sid}, method="GET")
                with urllib.request.urlopen(req, timeout=60) as r:
                    for line in r:
                        if b"tools/list_changed" in line:
                            seen["n"] += 1
            except Exception:
                pass
        threading.Thread(target=listen, daemon=True).start()
        time.sleep(0.5)
        e, t, _, _ = call("plugin", action="new", name="demo", description="demo plugin")
        check("plugin new", not e and "running" in t, t)
        e, t, _, _ = call("demo_hello", who="tester")
        check("scaffold tool works", not e and t == "hello tester from demo", t)
        t0 = time.time()
        open(os.path.join(PL, "demo", "server.py"), "w", encoding="utf-8").write(DEMO_V2)
        T = wait_for(lambda: (lambda T: T if "demo_add" in T else None)(tools()), 10)
        dt = time.time() - t0
        check(f"hot reload picked up new tools ({dt:.1f}s)", T and "demo_add" in T and "demo_hello" not in T and dt < 5, list(T or {}))
        check("list_changed notification", wait_for(lambda: seen["n"] > 0, 5), seen)
        s = T["demo_add"]["inputSchema"]
        check("schema from signature", s["properties"]["a"] == {"type": "integer", "description": "first"} and s.get("required") == ["a"] and s["properties"]["b"].get("default") == 0, s)
        check("description from docstring", T["demo_add"]["description"] == "[plugin demo] Add numbers.", T["demo_add"]["description"])
        s = T["demo_pick"]["inputSchema"]
        check("Literal/Optional schema", s["properties"]["mode"].get("enum") == ["x", "y"] and s["properties"]["tags"].get("type") == "array" and s.get("required") == ["mode"], s)
        check("ctx not in schema", "ctx" not in T["demo_slow"]["inputSchema"]["properties"], T["demo_slow"]["inputSchema"])

        print("== calls")
        e, t, _, _ = call("demo_add", a=2, b=40); check("call add", not e and t == "42", t)
        e, t, _, _ = call("demo_pick", mode="y", tags=[1]); check("dict result as json", not e and json.loads(t) == {"mode": "y", "tags": [1]}, t)
        e, t, _, m = call("demo_slow", _meta={"progressToken": "tok1"}, steps=4)
        progress = [x for x in m["_all"] if x.get("method") == "notifications/progress"]
        check("progress forwarded", not e and t == "done" and len(progress) == 4 and progress[-1]["params"]["progressToken"] == "tok1" and progress[0]["params"]["message"] == "step 0", (t, progress[:2]))
        imgp = os.path.join(HOME, "t.png"); png(imgp)
        e, t, imgs, _ = call("demo_img", path=imgp); check("image by path -> data", not e and len(imgs) == 1 and imgs[0].get("data") and "an image" in t, (t, imgs and list(imgs[0])))
        e, t, _, _ = call("demo_bad"); check("ToolError -> isError", e and t == "nope, on purpose", t)
        e, t, _, _ = call("demo_boom"); check("exception -> isError with type", e and "ZeroDivisionError" in t, t)
        e, t, _, _ = call("demo_noisy"); check("print does not break protocol", not e and t == "quiet", t)
        e, t, _, _ = call("demo_callback", path=HOME); check("plugin -> nerve tool callback", not e and "nerve-p5" in t and "dir" in t.lower(), t)
        e, t, _, _ = call("demo_add", a=1, zzz=5); check("unknown arg ignored with note", not e and t.startswith("1") and "zzz" in t, t)
        # concurrency: two slow calls in parallel
        res = {}
        def par(k):
            res[k] = call("demo_slow", steps=5)[1]
        th = [threading.Thread(target=par, args=(k,)) for k in range(3)]
        t0 = time.time(); [x.start() for x in th]; [x.join() for x in th]
        check(f"parallel calls ({time.time() - t0:.1f}s)", all(v == "done" for v in res.values()) and time.time() - t0 < 2.5, res)

        print("== crash / restart")
        e, t, _, _ = call("demo_crash"); check("crash -> error", e and "exited" in t, t)
        e, t, _, _ = call("demo_add", a=5); check("auto-restart after crash", not e and t == "5", t)
        e, t, _, _ = call("plugin", action="logs", name="demo"); check("plugin logs", not e and "hello from print" in t and "process exited" in t, t[-500:])

        print("== broken plugin / recovery")
        open(os.path.join(PL, "demo", "server.py"), "w", encoding="utf-8").write(DEMO_V2.replace("def add(", "def add(:"))
        st = wait_for(lambda: (lambda s: s if s and s["state"] == "error" else None)(next((p for p in json.load(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health"))["plugins"] if p["name"] == "demo"), None)), 10)
        check("syntax error -> state error with details", st and "SyntaxError" in (st.get("error") or ""), st)
        check("broken plugin tools removed", "demo_add" not in tools())
        e, t, _, _ = call("blender", action="status"); check("other plugins unaffected", not e and "running" in t, t)
        open(os.path.join(PL, "demo", "server.py"), "w", encoding="utf-8").write(DEMO_V2)
        check("fixed -> back", wait_for(lambda: "demo_add" in tools(), 10))

        print("== enable / disable / remove")
        e, t, _, _ = call("plugin", action="disable", name="demo"); check("disable", not e and "demo_add" not in tools(), t)
        e, t, _, _ = call("demo_add", a=1); check("disabled tool unknown", e, t)
        e, t, _, _ = call("plugin", action="enable", name="demo"); check("enable", not e and "demo_add" in tools(), t)
        shutil.rmtree(os.path.join(PL, "demo"))
        check("removed folder -> tools gone", wait_for(lambda: "demo_add" not in tools(), 8))

        print("== unreal (status only)")
        e, t, _, _ = call("unreal", action="status"); check("unreal status", not e and '"running": false' in t and "5." in t, t)
        e, t, _, _ = call("unreal", action="nope"); check("unknown action error", e and "valid:" in t, t)

        if BLENDER:
            print("== blender (headless)")
            e, t, _, _ = call("blender", action="headless", code="import bpy\nprint('OBJ', sorted(o.type for o in bpy.data.objects))")
            check("headless one-shot", not e and "OBJ ['CAMERA', 'LIGHT', 'MESH']" in t, t)
            e, t, _, _ = call("blender", action="headless", code="raise SystemExit(2)")
            check("headless failure -> error", e, t)
            t0 = time.time()
            e, t, _, _ = call("blender", action="launch", background=True)
            check(f"launch headless session ({time.time() - t0:.1f}s)", not e and '"headless": true' in t, t)
            e, t, _, _ = call("blender", action="exec", code="bpy.ops.mesh.primitive_uv_sphere_add(radius=1.5, location=(3,0,1))\nC.active_object.name = 'Ball'\nx = 41\nx + 1")
            check("exec returns last expression", not e and t.strip() == "42", t)
            e, t, _, _ = call("blender", action="exec", code="x"); check("exec state persists", not e and t.strip() == "41", t)
            e, t, _, _ = call("blender", action="exec", code="print('hi'); result = [o.name for o in D.objects]")
            check("exec stdout + result var", not e and "hi" in t and "Ball" in t, t)
            e, t, _, _ = call("blender", action="exec", code="1/0"); check("exec error", e and "ZeroDivisionError" in t, t)
            e, t, _, _ = call("blender", action="scene"); check("scene summary", not e and '"Ball"' in t and '"camera": "Camera"' in t, t[:400])
            e, t, imgs, _ = call("blender", action="render", engine="WORKBENCH", resolution=[320, 240])
            check("render -> image", not e and len(imgs) == 1 and "320x240" in t, t)
            e, t, _, _ = call("blender", action="screenshot"); check("screenshot headless -> clear error", e and "render" in t, t)
            glb = os.path.join(HOME, "out", "ball.glb")
            e, t, _, _ = call("blender", action="export", file=glb); check("export glb", not e and os.path.exists(glb) and os.path.getsize(glb) > 1000, t)
            e, t, _, _ = call("blender", action="new", empty=True); check("new empty", not e and '"objects": 0' in t, t)
            e, t, _, _ = call("blender", action="import", file=glb); check("import glb", not e and "Ball" in t, t)
            blend = os.path.join(HOME, "out", "t.blend")
            e, t, _, _ = call("blender", action="save", file=blend); check("save .blend", not e and os.path.exists(blend), t)
            e, t, _, _ = call("blender", action="quit"); check("quit", not e, t)
            check("session gone", wait_for(lambda: "not running" in call("blender", action="status")[1] or '"running": false' in call("blender", action="status")[1], 15))

        print("== bundled update rules (restart)")
        tom = os.path.join(PL, "blender", "plugin.toml")
        open(tom, "a", encoding="utf-8").write("\n# my edit\n")
        os.remove(os.path.join(PL, "README.md"))
        stop_server(srv)
        srv = start_server()
        check("user-edited bundled file kept", "# my edit" in open(tom, encoding="utf-8").read())
        check("user-deleted bundled file not recreated", not os.path.exists(os.path.join(PL, "README.md")))
        check("plugins back after restart", "blender" in tools())
    finally:
        stop_server(srv)
    print(f"\n{ok} passed, {fail} failed")
    sys.exit(1 if fail else 0)


if __name__ == "__main__":
    main()
