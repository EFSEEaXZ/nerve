"""Desktop `ui` tool test against an isolated nerve instance.

usage: python tests/test_desktop.py [path\\to\\nerve.exe]
Starts nerve with its own data dir on 127.0.0.1:8400 (no tunnel, no tray) and drives a private
WinForms window (tests/fixtures/desktop_window.ps1). Never touches other apps; clipboard text is restored.
"""

import json, os, subprocess, sys, tempfile, time, urllib.request, shutil

EXE = sys.argv[1] if len(sys.argv) > 1 else r"target\release\nerve.exe"
PORT = 8400
URL = f"http://127.0.0.1:{PORT}/mcp"
HOME = os.path.join(tempfile.gettempdir(), "nerve-p3")
HERE = os.path.dirname(os.path.abspath(__file__))
MARK = "nerve desktop Привет 123 !@#"
TITLE = "nerve-p3-target"
ok = fail = 0
sid = None
rid = 0


def check(name, cond, extra=""):
    global ok, fail
    if cond:
        ok += 1; print(f"  PASS {name}")
    else:
        fail += 1; print(f"  FAIL {name} :: {str(extra)[:500]}")


def rpc(method, params=None):
    global sid, rid
    rid += 1
    body = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}).encode()
    h = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if sid:
        h["Mcp-Session-Id"] = sid
    with urllib.request.urlopen(urllib.request.Request(URL, data=body, headers=h, method="POST"), timeout=120) as r:
        sid = r.headers.get("Mcp-Session-Id") or sid
        raw = r.read().decode("utf-8", "replace")
    msgs = [json.loads(raw)] if raw.lstrip().startswith("{") else [json.loads(l[5:]) for l in raw.splitlines() if l.startswith("data:") and l[5:].strip()]
    for m in msgs:
        if m.get("id") == rid:
            return m
    raise RuntimeError(raw[:300])


def ui(**args):
    res = rpc("tools/call", {"name": "ui", "arguments": args}).get("result", {})
    text = "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
    return res.get("isError", False), text, [c for c in res.get("content", []) if c.get("type") == "image"]


def editor_value(hwnd):
    _, t, _ = ui(action="tree", hwnd=hwnd, limit=50)
    return next((l for l in t.splitlines() if '"Editor"' in l), "")


def main():
    shutil.rmtree(HOME, ignore_errors=True)
    os.makedirs(HOME, exist_ok=True)
    with open(os.path.join(HOME, "config.toml"), "w") as f:
        f.write(f'[server]\nlocal_addr = "127.0.0.1:{PORT}"\npublic_addr = ""\n[tunnel]\nprovider = "none"\n[ui]\ntray = false\ndashboard = false\n')
    srv = subprocess.Popen([EXE, "--home", HOME, "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    target = None
    clip0 = None
    try:
        for _ in range(50):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=1); break
            except Exception:
                time.sleep(0.2)
        rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "p3", "version": "1"}})
        tools = [t["name"] for t in rpc("tools/list")["result"]["tools"]]
        check("ui tool registered", "ui" in tools, tools)
        _, clip0, _ = ui(action="clipboard")

        e, t, imgs = ui(action="screenshot", max_bytes=60000)
        check("screenshot screen", not e and imgs, t); print("   ", t.splitlines()[0][:170])
        e, t, imgs = ui(action="screenshot", target="monitor:1", tile="2x2:1", max_bytes=60000)
        check("screenshot monitor tile", not e and imgs and "tile 2x2:1" in t, t)

        target = subprocess.Popen(["powershell", "-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File", os.path.join(HERE, "fixtures", "desktop_window.ps1")])
        hwnd = None
        for _ in range(60):
            time.sleep(0.25)
            _, t, _ = ui(action="windows")
            for line in t.splitlines():
                if f'"{TITLE}"' in line:
                    hwnd = int(line.split("hwnd")[1].split()[0])
            if hwnd:
                break
        check("test window in `windows`", hwnd)
        if not hwnd:
            return

        e, t, _ = ui(action="window", hwnd=hwnd, op="focus"); check("window focus", not e and "focused" in t, t)
        e, t, _ = ui(action="window", hwnd=hwnd, op="move", rect=[200, 150, 900, 600])
        check("window move exact", not e and "[200,150 900x600]" in t, t)

        t0 = time.time()
        e, t, _ = ui(action="tree", hwnd=hwnd)
        check("tree: button + editor", not e and '"Press me"' in t and '"Editor"' in t, t)
        print(f"    tree {(time.time()-t0)*1000:.0f}ms"); [print("     ", l[:150]) for l in t.splitlines()[:6]]

        e, t, _ = ui(action="click", hwnd=hwnd, text="Editor"); check("click element by name", not e and "Editor" in t, t)
        e, t, _ = ui(action="type", text=MARK); check("type (keys)", not e and "(keys)" in t, t)
        time.sleep(0.3)
        v = editor_value(hwnd); check("typed text exact (unicode+symbols)", MARK in v, v)
        ui(action="type", text="\nline2\tTAB"); time.sleep(0.2)
        v = editor_value(hwnd); check("newline/tab", "line2" in v, v)

        long = ("длинный paste test " * 30).strip()
        e, t, _ = ui(action="type", text="\n" + long); check("long text -> paste", not e and "(paste)" in t, t)
        time.sleep(0.3)
        _, c, _ = ui(action="clipboard"); check("paste restored clipboard", c == clip0, f"{clip0[:60]!r} vs {c[:60]!r}")

        e, t, _ = ui(action="ocr", hwnd=hwnd); check("ocr reads typed text", not e and "desktop" in t, t)
        print("    ocr:", t.splitlines()[0][:120])
        e, t, _ = ui(action="ocr", hwnd=hwnd, lang="ru", filter="мир"); check("ocr russian", not e and "мир" in t.lower(), t)
        e, t, imgs = ui(action="screenshot", hwnd=hwnd, max_bytes=80000); check("screenshot window", not e and imgs, t)
        print("   ", t.splitlines()[0][:200])

        e, t, _ = ui(action="key", hwnd=hwnd, keys="ctrl+home ctrl+shift+end ctrl+c"); check("key chords", not e and "pressed" in t, t)
        time.sleep(0.3)
        _, t, _ = ui(action="clipboard"); check("ctrl+c -> clipboard", MARK in t, t)
        ui(action="clipboard", set="nerve clip ✓ тест")
        _, t, _ = ui(action="clipboard"); check("clipboard set/get", "nerve clip ✓ тест" in t, t)

        e, t, _ = ui(action="click", hwnd=hwnd, text="Press me"); check("click button by text", not e and "Press me" in t, t)
        time.sleep(0.4)
        _, t, _ = ui(action="windows"); check("button click took effect", f'"{TITLE} clicked"' in t, t)

        cx, cy = 650, 530
        e, t, _ = ui(action="move", x=cx, y=cy); check("mouse move", not e and f"({cx},{cy})" in t, t)
        e, t, _ = ui(action="cursor"); check("cursor", f"({cx},{cy})" in t, t)
        e, t, _ = ui(action="click", x=cx, y=cy, count=2); check("double click", not e and "double-clicked" in t, t)
        e, t, _ = ui(action="click", x=cx, y=cy, button="right"); check("right click", not e and "right clicked" in t, t)
        ui(action="key", keys="esc")
        e, t, _ = ui(action="scroll", x=cx, y=cy, dy=-3); check("scroll", not e, t)
        e, t, _ = ui(action="drag", x=cx - 100, y=cy, to_x=cx + 100, to_y=cy); check("drag", not e and "dragged" in t, t)

        e, t, _ = ui(action="window", hwnd=hwnd, op="minimize"); check("minimize", not e, t)
        e, t, imgs = ui(action="screenshot", hwnd=hwnd, max_bytes=40000); check("screenshot minimized window", not e and imgs, t)
        e, t, _ = ui(action="nope"); check("unknown action -> error", e, t)
        e, t, _ = ui(action="click"); check("click w/o target -> error", e, t)
        e, t, _ = ui(action="window", title="no-such-window-xyz"); check("missing window -> error", e, t)
        ui(action="window", hwnd=hwnd, op="close"); time.sleep(0.8)
        check("window close", target.poll() is not None)
    finally:
        if clip0 and ":\n" in clip0:
            try:
                ui(action="clipboard", set=clip0.split(":\n", 1)[1])
            except Exception:
                pass
        if target and target.poll() is None:
            target.kill()
        srv.terminate()
        try:
            srv.wait(5)
        except Exception:
            srv.kill()
    print(f"\n{ok} passed, {fail} failed")
    sys.exit(1 if fail else 0)


if __name__ == "__main__":
    main()
