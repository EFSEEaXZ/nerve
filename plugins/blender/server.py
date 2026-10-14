"""nerve plugin: drive Blender (live GUI session, headless session, or one-shot scripts)."""

import glob
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from nerve_plugin import Image, Plugin, ToolError, spawn_detached

HERE = os.path.dirname(os.path.abspath(__file__))
BRIDGE = os.path.join(HERE, "nerve_bridge.py")
KIT = os.path.join(HERE, "nerve_kit.py")
PORT = int(os.environ.get("NERVE_BLENDER_PORT", "9886"))
OUT = os.path.join(os.environ.get("NERVE_OUT") or tempfile.gettempdir(), "blender")
os.makedirs(OUT, exist_ok=True)

p = Plugin("blender", version="1.0.0")


# ------------------------------------------------------------------ locate Blender

def _ver(path):
    m = re.search(r"(\d+)\.(\d+)(?:\.(\d+))?", path)
    return tuple(int(x or 0) for x in m.groups()) if m else (0, 0, 0)


def find_blender():
    env = os.environ.get("BLENDER_PATH", "").strip()
    if env:
        if os.path.isfile(env):
            return env
        raise ToolError(f"BLENDER_PATH={env} does not exist")
    cands = []
    if os.name == "nt":
        for root in filter(None, [os.environ.get("ProgramFiles"), os.environ.get("ProgramFiles(x86)"), os.environ.get("LOCALAPPDATA") and os.path.join(os.environ["LOCALAPPDATA"], "Programs")]):
            cands += glob.glob(os.path.join(root, "Blender Foundation", "Blender*", "blender.exe"))
        cands += glob.glob(r"C:\Program Files (x86)\Steam\steamapps\common\Blender\blender.exe")
        cands += glob.glob(r"*:\SteamLibrary\steamapps\common\Blender\blender.exe")
    elif sys.platform == "darwin":
        cands += glob.glob("/Applications/Blender*.app/Contents/MacOS/Blender")
    else:
        cands += glob.glob("/opt/blender*/blender") + glob.glob(os.path.expanduser("~/blender*/blender")) + ["/snap/bin/blender"]
    cands = [c for c in cands if os.path.isfile(c)]
    if cands:
        return sorted(cands, key=_ver)[-1]
    w = shutil.which("blender")
    if w:
        return w
    raise ToolError("Blender not found. Install it or set BLENDER_PATH in plugins/blender/plugin.toml [env].")


# ------------------------------------------------------------------ bridge client

def bridge(op, timeout=600, **kw):
    req = dict(kw, op=op, id=1, timeout=timeout, kit=KIT)
    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=3)
    except OSError:
        raise ToolError("Blender is not running with the nerve bridge. Use action `launch` (or `install_addon` once, then open Blender normally).")
    try:
        s.settimeout(timeout + 5)
        s.sendall((json.dumps(req) + "\n").encode())
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    finally:
        s.close()
    if not buf:
        raise ToolError("Blender closed the connection (did it quit or crash?)")
    rep = json.loads(buf)
    if not rep.get("ok"):
        raise ToolError(rep.get("error", "error") + ("\n" + rep["trace"] if rep.get("trace") and "RuntimeError" not in rep.get("error", "") else ""))
    return rep.get("data")


def alive():
    try:
        return bridge("ping", timeout=5)
    except ToolError:
        return None


# ------------------------------------------------------------------ actions

def do_status(a, ctx):
    info = alive()
    if info:
        return {"running": True, **info}
    try:
        exe = find_blender()
    except ToolError as e:
        exe = str(e)
    return {"running": False, "blender": exe, "hint": "action launch starts Blender with the bridge"}


def do_launch(a, ctx):
    info = alive()
    if info:
        return {"already_running": True, **info}
    exe = find_blender()
    bg = bool(a.get("background"))
    argv = [exe]
    if bg:
        argv.append("-b")
    if a.get("file"):
        argv.append(a["file"])
    argv += ["--python", BRIDGE]
    env = dict(os.environ, NERVE_BLENDER_PORT=str(PORT), NERVE_BLENDER_OUT=OUT)
    if bg:
        env["NERVE_BLENDER_SERVE"] = "1"
    log = os.path.join(OUT, "blender-session.log")
    proc = spawn_detached(argv, env=env, log=log, hidden=bg)
    deadline = time.time() + float(a.get("timeout_s") or 120)
    while time.time() < deadline:
        ctx.check()
        if proc.poll() is not None:
            tail = open(log, encoding="utf-8", errors="replace").read()[-2000:]
            raise ToolError(f"Blender exited with code {proc.returncode}:\n{tail}")
        info = alive()
        if info:
            return {"launched": True, "headless": bg, **info}
        ctx.progress("waiting for Blender to start...")
        time.sleep(0.5)
    raise ToolError(f"Blender started (pid {proc.pid}) but the bridge did not answer in time; see {log}")


def _code_result(r):
    parts = []
    if r.get("stdout"):
        parts.append(r["stdout"].rstrip())
    if r.get("result") is not None:
        v = r["result"]
        parts.append(v if isinstance(v, str) else json.dumps(v, ensure_ascii=False, indent=1))
    return "\n".join(parts) or "ok"


def do_exec(a, ctx):
    code = a.get("code")
    if not code:
        raise ToolError("`code` is required")
    return _code_result(bridge("exec", code=code, timeout=float(a.get("timeout_s") or 600)))


def do_scene(a, ctx):
    return bridge("scene", filter=a.get("filter"), limit=a.get("limit") or 300)


def do_screenshot(a, ctx):
    r = bridge("screenshot", view=a.get("view"), shading=a.get("shading"))
    return [Image(r["path"], max_bytes=a.get("max_bytes")), f"{r['what']} screenshot: {r['path']}"]


def do_render(a, ctx):
    ctx.progress("rendering...")
    r = bridge("render", engine=a.get("engine"), samples=a.get("samples"), resolution=a.get("resolution"), frame=a.get("frame"), output=a.get("output"), timeout=float(a.get("timeout_s") or 3600))
    return [Image(r["path"], max_bytes=a.get("max_bytes")), f"rendered {r['size'][0]}x{r['size'][1]} with {r['engine']} in {r['seconds']}s -> {r['path']}"]


def do_sheet(a, ctx):
    """Model sheet: the same objects from several angles at one scale (render / clay / wire)."""
    ctx.progress("rendering the sheet...")
    keys = ("objects", "views", "mode", "size", "focus", "lens", "elevation", "cols", "samples")
    kw = {k: a[k] for k in keys if a.get(k) is not None}
    if a.get("output"):
        kw["out"] = os.path.abspath(a["output"])
    r = bridge("sheet", timeout=float(a.get("timeout_s") or 900), **kw)
    return [Image(r["path"], max_bytes=a.get("max_bytes")), f"{a.get('mode') or 'render'} sheet {', '.join(r['cells'])} ({r['cell'][0]}x{r['cell'][1]} per view) in {r['seconds']}s -> {r['path']}"]


def do_topo(a, ctx):
    return bridge("topo", object=a.get("object"))


def do_kit(a, ctx):
    """List the kit helpers available as `kit` inside exec code."""
    return bridge("kit")


def do_open(a, ctx):
    if not a.get("file"):
        raise ToolError("`file` is required")
    return bridge("open", file=os.path.abspath(a["file"]))


def do_save(a, ctx):
    return bridge("save", file=a.get("file") and os.path.abspath(a["file"]))


def do_new(a, ctx):
    return bridge("new", empty=bool(a.get("empty")))


def do_import(a, ctx):
    if not a.get("file"):
        raise ToolError("`file` is required")
    return bridge("import", file=os.path.abspath(a["file"]))


def do_export(a, ctx):
    if not a.get("file"):
        raise ToolError("`file` is required (extension picks the format: .fbx .obj .glb .gltf .stl .ply .usd .abc)")
    return bridge("export", file=os.path.abspath(a["file"]), selected=bool(a.get("selected")))


def do_quit(a, ctx):
    if not alive():
        return "Blender is not running"
    return bridge("quit", timeout=10)


def do_headless(a, ctx):
    """One-shot: run code in a fresh background Blender and return its output."""
    code = a.get("code")
    if not code:
        raise ToolError("`code` is required")
    exe = find_blender()
    script = os.path.join(OUT, f"oneshot-{int(time.time() * 1000)}.py")
    with open(script, "w", encoding="utf-8") as f:
        f.write(code)
    argv = [exe, "-b"] + ([a["file"]] if a.get("file") else []) + ["--python-exit-code", "1", "--python", script]
    try:
        r = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=float(a.get("timeout_s") or 600), creationflags=0x08000000 if os.name == "nt" else 0)
    except subprocess.TimeoutExpired:
        raise ToolError("timed out")
    finally:
        try:
            os.remove(script)
        except OSError:
            pass
    out = (r.stdout or "") + (r.stderr or "")
    noise = ("Blender ", "Read prefs", "Read blend", "Blender quit", "\nBlender quit", "Color management", "Warning: File written by newer")
    lines = [l for l in out.splitlines() if not l.startswith(noise)]
    text = "\n".join(lines).strip()[-15000:]
    if r.returncode != 0:
        raise ToolError(f"exit {r.returncode}\n{text}")
    return text or "ok"


def do_install_addon(a, ctx):
    """Install the bridge as a Blender add-on so every Blender you open by hand is controllable."""
    exe = find_blender()
    probe = "import bpy;print('NERVE_ADDONS=' + bpy.utils.user_resource('SCRIPTS', path='addons', create=True))"
    r = subprocess.run([exe, "-b", "--factory-startup", "--python-expr", probe], stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", timeout=120, creationflags=0x08000000 if os.name == "nt" else 0)
    m = re.search(r"NERVE_ADDONS=(.+)", r.stdout or "")
    if not m:
        raise ToolError("could not find Blender's add-on folder:\n" + (r.stdout + r.stderr)[-2000:])
    dst_dir = m.group(1).strip()
    os.makedirs(dst_dir, exist_ok=True)
    dst = os.path.join(dst_dir, "nerve_bridge.py")
    shutil.copyfile(BRIDGE, dst)
    enable = "import bpy, addon_utils; addon_utils.enable('nerve_bridge', default_set=True, persistent=True); bpy.ops.wm.save_userpref(); print('NERVE_OK')"
    r = subprocess.run([exe, "-b", "--python-expr", enable], stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", timeout=120, creationflags=0x08000000 if os.name == "nt" else 0)
    if "NERVE_OK" not in (r.stdout or ""):
        raise ToolError(f"copied to {dst} but enabling failed:\n" + (r.stdout + r.stderr)[-2000:])
    return f"installed and enabled: {dst}\nEvery Blender you open now listens on 127.0.0.1:{PORT} (restart an already open Blender)."


ACTIONS = {k[3:]: v for k, v in globals().items() if k.startswith("do_")}

SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": sorted(ACTIONS), "description": "What to do (see tool description)."},
        "code": {"type": "string", "description": "exec/headless: Python run inside Blender (bpy, mathutils, Vector, C=context, D=data preloaded; the value of the last expression or a `result` variable is returned; state persists between exec calls)."},
        "file": {"type": "string", "description": "launch/open/headless: .blend to open; save: target path; import/export: file (extension picks the format)."},
        "background": {"type": "boolean", "description": "launch: headless session without a window (render/exec only, no screenshots)."},
        "view": {"type": "string", "enum": ["camera", "all", "selected", "front", "back", "left", "right", "top", "bottom"], "description": "screenshot: frame the viewport first."},
        "shading": {"type": "string", "enum": ["WIREFRAME", "SOLID", "MATERIAL", "RENDERED"], "description": "screenshot: temporary viewport shading."},
        "engine": {"type": "string", "description": "render: EEVEE | CYCLES | WORKBENCH."},
        "samples": {"type": "integer", "description": "render: sample count."},
        "resolution": {"type": "array", "items": {"type": "integer"}, "description": "render: [width, height]."},
        "frame": {"type": "integer", "description": "render: frame number."},
        "output": {"type": "string", "description": "render: output PNG path (default: nerve out dir)."},
        "selected": {"type": "boolean", "description": "export: only selected objects."},
        "empty": {"type": "boolean", "description": "new: start from an empty scene instead of the default cube."},
        "filter": {"type": "string", "description": "scene: only objects whose name/type contains this."},
        "limit": {"type": "integer", "description": "scene: max objects listed (default 300)."},
        "max_bytes": {"type": "integer", "description": "screenshot/render: image size budget."},
        "timeout_s": {"type": "number", "description": "launch/exec/render/headless: timeout in seconds."},
        "objects": {"type": "array", "items": {"type": "string"}, "description": "sheet: objects to show (default: all visible)."},
        "views": {"type": "array", "items": {"type": "string"}, "description": "sheet: front | 34 | side | back | 34back | left | azimuth degrees (default front, 34, side, back)."},
        "mode": {"type": "string", "enum": ["render", "clay", "wire"], "description": "sheet: render = scene materials and lights; clay = matcap + cavity to judge form; wire = cage + edges to judge topology."},
        "size": {"type": "integer", "description": "sheet: cell height in px (default 560)."},
        "focus": {"type": "array", "items": {"type": "string"}, "description": "sheet: frame only these objects (e.g. the head) while showing all."},
        "lens": {"type": "number", "description": "sheet: perspective lens in mm (default orthographic)."},
        "elevation": {"type": "number", "description": "sheet: camera elevation in degrees."},
        "object": {"type": "string", "description": "topo: mesh object (default: active)."},
    },
    "required": ["action"],
}


@p.tool(
    schema=SCHEMA,
    description=(
        "Blender 3D. Actions: status; launch {file?, background?} (starts Blender with the nerve bridge; a GUI window by default); "
        "exec {code} (Python in the live Blender: build/modify scenes, materials, modifiers, animation...); "
        "scene {filter?} (objects, transforms, materials, camera, render settings); screenshot {view?, shading?} (3D viewport image); "
        "render {engine?, samples?, resolution?, frame?, output?} (still render, returns the image); open/save {file}; new {empty?}; "
        "import/export {file, selected?} (fbx obj glb gltf stl ply usd abc; .blend append on import); quit; "
        "headless {code, file?} (one-shot background Blender, returns stdout; no running session needed); "
        "install_addon (make every manually opened Blender controllable); "
        "sheet {objects?, views?, mode?, size?, focus?, lens?} (model sheet: several angles side by side at one scale, as render | clay | wire); "
        "topo {object?} (topology report: quads/tris/ngons, poles, open/non-manifold edges, flipped normals, symmetry); "
        "kit (list the `kit` helpers usable in exec: loft/sweep/spline mesh building, sculpt brushes grab/inflate/relax..., toon_material, outline, sheet, topo). "
        "Typical loop: launch -> exec (kit...) -> sheet -> adjust."
    ),
    annotations={"title": "Blender", "destructiveHint": True, "openWorldHint": False},
)
def blender(ctx, **a):
    act = a.get("action") or "status"
    fn = ACTIONS.get(act)
    if fn is None:
        raise ToolError(f"unknown action `{act}`; valid: {', '.join(sorted(ACTIONS))}")
    return fn(a, ctx)


if __name__ == "__main__":
    p.run()
