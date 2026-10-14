"""`browser` tool test against an isolated nerve instance.

usage: python tests/test_browser.py [path\\to\\nerve.exe]
Starts nerve with its own data dir on 127.0.0.1:8400 (no tunnel, no tray) and a *headless* browser with
its own throwaway profile, driving local test pages (tests/fixtures/browser_page.html). Never touches your browser.
"""

import json, os, re, subprocess, sys, tempfile, time, urllib.request, shutil

EXE = sys.argv[1] if len(sys.argv) > 1 else r"target\release\nerve.exe"
PORT = 8400
URL = f"http://127.0.0.1:{PORT}/mcp"
HOME = os.path.join(tempfile.gettempdir(), "nerve-p4")
HERE = os.path.dirname(os.path.abspath(__file__))
PAGE = os.path.join(HERE, "fixtures", "browser_page.html")
ok = fail = 0
sid = None
rid = 0


def check(name, cond, extra=""):
    global ok, fail
    if cond:
        ok += 1; print(f"  PASS {name}")
    else:
        fail += 1; print(f"  FAIL {name} :: {str(extra)[:700]}")


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


def br(**args):
    t0 = time.time()
    res = rpc("tools/call", {"name": "browser", "arguments": args}).get("result", {})
    text = "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
    ms = (time.time() - t0) * 1000
    return res.get("isError", False), text, [c for c in res.get("content", []) if c.get("type") == "image"], ms


def ev(js):
    e, t, _, _ = br(action="eval", js=js)
    return t if not e else "ERR:" + t


def ref_of(snap, pattern):
    for line in snap.splitlines():
        if re.search(pattern, line):
            m = re.match(r"\s*\[(\d+)\]", line)
            if m:
                return int(m.group(1))
    return None


def start_server():
    p = subprocess.Popen([EXE, "--home", HOME, "serve"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=1); break
        except Exception:
            time.sleep(0.2)
    global sid
    sid = None
    rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "p4", "version": "1"}})
    return p


def main():
    shutil.rmtree(HOME, ignore_errors=True)
    os.makedirs(HOME, exist_ok=True)
    with open(os.path.join(HOME, "config.toml"), "w") as f:
        f.write(f'[server]\nlocal_addr = "127.0.0.1:{PORT}"\npublic_addr = ""\n[tunnel]\nprovider = "none"\n[ui]\ntray = false\ndashboard = false\n[browser]\nheadless = true\n')
    srv = start_server()
    try:
        tools = [t["name"] for t in rpc("tools/list")["result"]["tools"]]
        check("browser tool registered", "browser" in tools, tools)
        e, t, _, _ = br(action="status"); check("status before start", not e and "not running" in t, t)

        e, t, _, ms = br(action="open", url=PAGE)
        check("open local page", not e and "nerve p4 page" in t and "loaded" in t, t); print(f"    open {ms:.0f}ms:", t.replace("\n", " | ")[:200])
        e, t, _, _ = br(action="status"); check("status running headless", not e and "launched" in t and "headless" in t, t)

        e, snap, _, ms = br(action="snapshot")
        print(f"    snapshot {ms:.0f}ms, {len(snap)} chars"); [print("     ", l[:140]) for l in snap.splitlines()[:16]]
        check("snapshot heading+text", "# Test page" in snap and "zebra-42" in snap, snap)
        r_name, r_color, r_agree = ref_of(snap, r'textbox "Name"'), ref_of(snap, r'select "Color"'), ref_of(snap, r'checkbox')
        r_go, r_second, r_newtab = ref_of(snap, r'button "Go"'), ref_of(snap, r'link "Second page"'), ref_of(snap, r'link "New tab link"')
        check("snapshot refs", all(x for x in [r_name, r_color, r_agree, r_go, r_second, r_newtab]), snap)
        check("snapshot select options", "Red | Green | Blue" in snap, snap)
        check("snapshot offscreen marker", re.search(r'Bottom of page', snap) is not None, snap)

        e, t, _, _ = br(action="type", ref=r_name, text="Привет nerve ✓"); check("type by ref", not e and "typed" in t, t)
        check("typed value exact", ev("document.querySelector('#name').value") == "Привет nerve ✓")
        br(action="type", ref=r_name, text="!", clear=False)
        check("type append (clear=false)", ev("document.querySelector('#name').value") == "Привет nerve ✓!")
        e, t, _, _ = br(action="select", ref=r_color, value="Green"); check("select by label", not e and ev("document.querySelector('#color').value") == "green", t)
        e, t, _, _ = br(action="click", ref=r_agree); check("click checkbox", not e and ev("String(document.querySelector('#agree').checked)") == "true", t)

        e, t, _, ms = br(action="click", text="Go")
        check("click by text", not e and "clicked" in t, t); print(f"    click {ms:.0f}ms:", t.replace("\n", " | ")[:240])
        check("click effect", ev("document.querySelector('#out').textContent") == "done: Привет nerve ✓! / green / true")
        check("dialog auto-accepted + reported", "[dialog alert]" in t, t)
        e, t, _, _ = br(action="console"); check("console log+error captured", "[log] go clicked" in t and "sample error" in t, t)

        br(action="type", selector="#notes", text="abc def")
        e, t, _, _ = br(action="key", selector="#notes", keys="ctrl+a Backspace")
        check("key ctrl+a backspace", not e and ev("document.querySelector('#notes').value") == "", t)
        br(action="type", selector="#notes", text="line1")
        br(action="key", keys="Enter")
        br(action="type", selector="#notes", text="line2", clear=False)
        check("key Enter in textarea", ev("document.querySelector('#notes').value") == "line1\nline2", ev("JSON.stringify(document.querySelector('#notes').value)"))
        e, t, _, _ = br(action="type", selector="#ed", text="rich text ok")
        check("type contenteditable", ev("document.querySelector('#ed').innerText.trim()") == "rich text ok", t)

        e, t, _, _ = br(action="click", text="Later")
        e, t, _, ms = br(action="wait", text="late arrival", timeout_ms=5000); check("wait for text", not e and "ok after" in t, t)
        e, t, _, _ = br(action="wait", text="never-appears-xyz", timeout_ms=600); check("wait timeout is an error", e and "timed out" in t, t)

        e, t, _, _ = br(action="eval", js="const a = 2; return a * 21"); check("eval with return", not e and t.strip() == "42", t)
        e, t, _, _ = br(action="eval", js="await new Promise(r => setTimeout(r, 50)); ({x: 1, y: [2, 3]})"); check("eval await + object", not e and '"y"' in t, t)
        e, t, _, _ = br(action="eval", js="nope.boom()"); check("eval error is error", e and "nope" in t, t)

        e, t, _, _ = br(action="upload", selector="#file", paths=[os.path.abspath(__file__)]); check("upload file", not e and ev("document.querySelector('#file').files[0].name") == "test_browser.py", t)
        e, t, _, _ = br(action="text", max_chars=300); check("text", not e and "zebra-42" in t, t)
        e, t, _, _ = br(action="html", selector="#name"); check("html selector", not e and 'id="name"' in t, t)

        e, t, _, _ = br(action="scroll", dy=700); check("scroll wheel", not e and re.search(r"y=(\d+)", t) and int(re.search(r"y=(\d+)", t).group(1)) > 0, t)
        e, t, _, _ = br(action="scroll", selector="#bottom"); check("scroll into view", not e and "into view" in t, t)

        e, t, imgs, ms = br(action="screenshot"); check("screenshot viewport", not e and imgs and "viewport" in t, t); print(f"    shot {ms:.0f}ms:", t.replace("\n", " | ")[:200])
        e, t, imgs, _ = br(action="screenshot", full_page=True, max_bytes=60000); check("screenshot full page", not e and imgs and "full page" in t, t)
        e, t, imgs, _ = br(action="screenshot", selector="#go"); check("screenshot element", not e and imgs and "element" in t, t); print("   ", t.replace("\n", " | ")[:200])

        e, t, _, _ = br(action="scroll", selector="h1")
        e, snap, _, _ = br(action="snapshot", interactive_only=True)
        check("interactive_only has no text", "zebra-42" not in snap and "Second page" in snap, snap)
        r_second = ref_of(snap, r'link "Second page"')
        e, t, _, ms = br(action="click", ref=r_second)
        check("click link navigates", not e and "navigated" in t and "nerve p4 second" in t, t); print(f"    nav {ms:.0f}ms:", t.replace("\n", " | ")[:200])
        e, t, _, _ = br(action="click", ref=r_go); check("stale ref is a clear error", e and "snapshot" in t, t)
        e, t, _, _ = br(action="back"); check("back", not e and "nerve p4 page" in t, t)

        e, snap, _, _ = br(action="snapshot", filter="new tab")
        check("snapshot filter", "New tab link" in snap and "zebra" not in snap, snap)
        e, t, _, _ = br(action="click", ref=ref_of(snap, r'New tab link'))
        check("target=_blank -> new tab followed", not e and "new tab" in t and "nerve p4 second" in t, t)
        e, t, _, _ = br(action="tabs"); check("tabs lists 2", not e and t.startswith("2 tab"), t)
        e, t, _, _ = br(action="close"); check("close tab", not e and "1 left" in t, t)
        e, t, _, _ = br(action="open", url=os.path.join(HERE, "fixtures", "browser_second.html"), new_tab=True)
        e, t, _, _ = br(action="tabs"); check("open new_tab", t.startswith("2 tab"), t)
        e, t, _, _ = br(action="tab", index=1); check("switch tab", not e and "nerve p4 page" in t, t)
        br(action="close", index=2)

        # nerve restart: the browser keeps running and is adopted
        srv.terminate(); srv.wait(10)
        srv = start_server()
        e, t, _, _ = br(action="snapshot", max_chars=500); check("adopted after nerve restart", not e and "Test page" in t, t)
        e, t, _, _ = br(action="status"); check("status adopted", not e and "adopted" in t, t)
        e, t, _, _ = br(action="open", url="http://127.0.0.1:1/"); check("unreachable url is an error", e and "could not open" in t, t)
        e, t, _, _ = br(action="quit"); check("quit", not e and "closed" in t, t)
        time.sleep(1)
        e, t, _, _ = br(action="status"); check("status after quit", "not running" in t, t)
    finally:
        try:
            br(action="quit")
        except Exception:
            pass
        srv.terminate()
        try:
            srv.wait(10)
        except Exception:
            srv.kill()
    print(f"{ok} passed, {fail} failed")
    return 1 if fail else 0


if __name__ == "__main__":
    sys.exit(main())
