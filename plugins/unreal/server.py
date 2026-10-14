"""nerve plugin: drive the Unreal Editor (live session via a Python bridge inside the editor)."""

import glob
import json
import os
import re
import shutil
import socket
import sys
import tempfile
import time

from nerve_plugin import Image, Plugin, ToolError, spawn_detached

HERE = os.path.dirname(os.path.abspath(__file__))
BRIDGE = os.path.join(HERE, "nerve_unreal_bridge.py")
PORT = int(os.environ.get("NERVE_UNREAL_PORT", "9877"))
OUT = os.path.join(os.environ.get("NERVE_OUT") or tempfile.gettempdir(), "unreal")
os.makedirs(OUT, exist_ok=True)
NEEDED_PLUGINS = ["PythonScriptPlugin", "EditorScriptingUtilities"]
MARK_BEGIN, MARK_END = "# >>> nerve bridge", "# <<< nerve bridge"

p = Plugin("unreal", version="1.0.0")


# ------------------------------------------------------------------ paths / engines

def documents():
    if os.name == "nt":
        try:
            import ctypes
            from ctypes import wintypes

            buf = ctypes.create_unicode_buffer(wintypes.MAX_PATH)
            ctypes.windll.shell32.SHGetFolderPathW(None, 5, None, 0, buf)  # CSIDL_PERSONAL
            if buf.value:
                return buf.value
        except Exception:
            pass
    return os.path.join(os.path.expanduser("~"), "Documents")


def user_python_dir():
    return os.path.join(documents(), "UnrealEngine", "Python")


def projects_dir():
    return os.path.join(documents(), "Unreal Projects")


def _ver(s):
    m = re.search(r"(\d+)\.(\d+)", s or "")
    return (int(m.group(1)), int(m.group(2))) if m else (0, 0)


def engines():
    """{version: root} of installed engines."""
    found = {}
    env = os.environ.get("UNREAL_ENGINE_ROOT", "").strip()
    if env:
        found["custom"] = env
    dat = r"C:\ProgramData\Epic\UnrealEngineLauncher\LauncherInstalled.dat"
    if os.path.isfile(dat):
        try:
            for it in json.load(open(dat, encoding="utf-8")).get("InstallationList", []):
                if re.fullmatch(r"UE_\d+\.\d+", it.get("AppName", "")):
                    found[it["AppName"][3:]] = it["InstallLocation"]
        except Exception:
            pass
    for root in glob.glob(r"C:\Program Files\Epic Games\UE_*") + glob.glob("/Users/Shared/Epic Games/UE_*"):
        found.setdefault(os.path.basename(root)[3:], root)
    return {k: v for k, v in found.items() if os.path.isfile(editor_exe(v))}


def editor_exe(root):
    if os.name == "nt":
        return os.path.join(root, "Engine", "Binaries", "Win64", "UnrealEditor.exe")
    if sys.platform == "darwin":
        return os.path.join(root, "Engine", "Binaries", "Mac", "UnrealEditor.app", "Contents", "MacOS", "UnrealEditor")
    return os.path.join(root, "Engine", "Binaries", "Linux", "UnrealEditor")


def pick_engine(assoc=None):
    e = engines()
    if not e:
        raise ToolError("no Unreal Engine install found (set UNREAL_ENGINE_ROOT in plugins/unreal/plugin.toml [env])")
    if assoc and assoc in e:
        return assoc, e[assoc]
    if "custom" in e:
        return "custom", e["custom"]
    v = sorted(e, key=_ver)[-1]
    return v, e[v]


def list_projects():
    out = []
    for d in [projects_dir()] + [x for x in os.environ.get("UNREAL_PROJECTS", "").split(os.pathsep) if x]:
        out += glob.glob(os.path.join(d, "*", "*.uproject"))
    return sorted(set(out), key=lambda p: -os.path.getmtime(p))


def resolve_project(name):
    if not name:
        ps = list_projects()
        if not ps:
            raise ToolError("no projects found; pass `project` (path to .uproject) or use new_project")
        return ps[0]
    if os.path.isfile(name) and name.endswith(".uproject"):
        return os.path.abspath(name)
    if os.path.isdir(name):
        g = glob.glob(os.path.join(name, "*.uproject"))
        if g:
            return g[0]
    for p_ in list_projects():
        if os.path.splitext(os.path.basename(p_))[0].lower() == name.lower():
            return p_
    raise ToolError(f"project `{name}` not found (looked in {projects_dir()})")


# ------------------------------------------------------------------ install

def ensure_project_plugins(uproject):
    with open(uproject, encoding="utf-8-sig") as f:
        data = json.load(f)
    plugins = data.setdefault("Plugins", [])
    changed = []
    for name in NEEDED_PLUGINS:
        ent = next((x for x in plugins if x.get("Name") == name), None)
        if ent is None:
            plugins.append({"Name": name, "Enabled": True})
            changed.append(name)
        elif not ent.get("Enabled"):
            ent["Enabled"] = True
            changed.append(name)
    if changed:
        with open(uproject, "w", encoding="utf-8") as f:
            json.dump(data, f, indent="\t")
    return changed


def install_bridge():
    d = user_python_dir()
    os.makedirs(d, exist_ok=True)
    shutil.copyfile(BRIDGE, os.path.join(d, "nerve_unreal_bridge.py"))
    init = os.path.join(d, "init_unreal.py")
    block = (
        f"{MARK_BEGIN}\n"
        "try:\n"
        "    import nerve_unreal_bridge\n"
        "    nerve_unreal_bridge.start()\n"
        "except Exception as _e:\n"
        "    import unreal\n"
        "    unreal.log_warning('nerve bridge failed: %s' % _e)\n"
        f"{MARK_END}\n"
    )
    text = open(init, encoding="utf-8").read() if os.path.exists(init) else ""
    if MARK_BEGIN in text:
        text = re.sub(re.escape(MARK_BEGIN) + r".*?" + re.escape(MARK_END) + r"\n?", block, text, flags=re.S)
    else:
        text = (text.rstrip() + "\n\n" if text.strip() else "") + block
    with open(init, "w", encoding="utf-8") as f:
        f.write(text)
    return d


# ------------------------------------------------------------------ bridge client

def bridge(op, timeout=600, **kw):
    req = dict(kw, op=op, id=1, timeout=timeout)
    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=3)
    except OSError:
        raise ToolError("the Unreal Editor is not running with the nerve bridge: use action `launch` (or `status`)")
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
        raise ToolError("the editor closed the connection")
    rep = json.loads(buf)
    if not rep.get("ok"):
        raise ToolError(rep.get("error", "error") + ("\n" + rep["trace"] if rep.get("trace") else ""))
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
    return {
        "running": False,
        "engines": engines(),
        "projects": list_projects()[:20],
        "bridge_installed": os.path.exists(os.path.join(user_python_dir(), "nerve_unreal_bridge.py")),
        "hint": "launch {project} opens the editor with the bridge; new_project {name} creates an empty project",
    }


def do_projects(a, ctx):
    return {"projects": list_projects(), "dir": projects_dir()}


def do_install(a, ctx):
    d = install_bridge()
    msg = [f"bridge installed in {d} (loaded by every editor at startup)"]
    if a.get("project"):
        up = resolve_project(a["project"])
        ch = ensure_project_plugins(up)
        msg.append(f"{os.path.basename(up)}: " + (f"enabled {', '.join(ch)}" if ch else "Python plugins already enabled"))
    return "\n".join(msg)


def do_new_project(a, ctx):
    name = a.get("name") or a.get("project")
    if not name or not re.fullmatch(r"[A-Za-z][A-Za-z0-9_]{0,19}", name):
        raise ToolError("`name` is required: letters/digits/_ only, starts with a letter, max 20 chars")
    ver, root = pick_engine(a.get("engine"))
    d = os.path.join(a.get("dir") or projects_dir(), name)
    up = os.path.join(d, name + ".uproject")
    if os.path.exists(up):
        raise ToolError(f"already exists: {up}")
    for sub in ("Content", "Config"):
        os.makedirs(os.path.join(d, sub), exist_ok=True)
    data = {
        "FileVersion": 3,
        "EngineAssociation": ver if ver != "custom" else "",
        "Category": "",
        "Description": "Created by nerve",
        "Plugins": [{"Name": n, "Enabled": True} for n in NEEDED_PLUGINS],
    }
    with open(up, "w", encoding="utf-8") as f:
        json.dump(data, f, indent="\t")
    with open(os.path.join(d, "Config", "DefaultEngine.ini"), "w", encoding="utf-8") as f:
        f.write("[/Script/EngineSettings.GameMapsSettings]\nEditorStartupMap=/Game/Main.Main\nGameDefaultMap=/Game/Main.Main\n\n"
                "[/Script/Engine.RendererSettings]\nr.DefaultFeature.AutoExposure=False\n")
    with open(os.path.join(d, "Config", "DefaultGame.ini"), "w", encoding="utf-8") as f:
        f.write(f"[/Script/EngineSettings.GeneralProjectSettings]\nProjectName={name}\n")
    install_bridge()
    res = {"created": up, "engine": ver}
    if a.get("launch", True):
        res["launch"] = do_launch(dict(a, project=up), ctx)
    return res


def do_launch(a, ctx):
    info = alive()
    if info:
        return {"already_running": True, **info}
    up = resolve_project(a.get("project"))
    assoc = json.load(open(up, encoding="utf-8-sig")).get("EngineAssociation")
    ver, root = pick_engine(assoc or a.get("engine"))
    ensure_project_plugins(up)
    install_bridge()
    argv = [editor_exe(root), up]
    if a.get("level"):
        argv.append(a["level"])
    argv += a.get("args") or []
    env = dict(os.environ, NERVE_UNREAL_PORT=str(PORT), NERVE_UNREAL_OUT=OUT)
    log = os.path.join(OUT, "editor-launch.log")
    proc = spawn_detached(argv, env=env, log=log)
    wait = float(a.get("timeout_s") or 50)
    deadline = time.time() + wait
    while time.time() < deadline:
        ctx.check()
        if proc.poll() is not None:
            raise ToolError(f"the editor exited with code {proc.returncode} (see {log} and the project's Saved/Logs)")
        info = alive()
        if info:
            if not str(info.get("level_path", "")).startswith("/Game/") and "Created by nerve" in open(up, encoding="utf-8-sig").read():
                try:
                    bridge("new_level", level="/Game/Main", timeout=60)
                    bridge("exec", code=_STARTER_SCENE, timeout=60)
                    bridge("save", timeout=120)
                    info = alive() or info
                except ToolError as e:
                    info["starter_scene_error"] = str(e)[:500]
            return {"launched": True, "engine": ver, **info}
        ctx.progress("editor is starting (first start of a project compiles shaders and can take minutes)...")
        time.sleep(1)
    return {"launched": True, "ready": False, "pid": proc.pid, "project": up,
            "note": f"the editor is still loading after {int(wait)}s; call action status until running=true"}


_STARTER_SCENE = """
eas = unreal.get_editor_subsystem(unreal.EditorActorSubsystem)
floor = eas.spawn_actor_from_object(unreal.EditorAssetLibrary.load_asset('/Engine/BasicShapes/Plane'), unreal.Vector(0, 0, 0))
floor.set_actor_scale3d(unreal.Vector(40, 40, 1)); floor.set_actor_label('Floor')
sun = eas.spawn_actor_from_class(unreal.DirectionalLight, unreal.Vector(0, 0, 500), unreal.Rotator(roll=0, pitch=-45, yaw=30)); sun.set_actor_label('Sun')
sky = eas.spawn_actor_from_class(unreal.SkyLight, unreal.Vector(0, 0, 300)); sky.set_actor_label('SkyLight')
for cls, label in ((unreal.SkyAtmosphere, 'SkyAtmosphere'), (unreal.ExponentialHeightFog, 'Fog'), (unreal.PlayerStart, 'PlayerStart')):
    a = eas.spawn_actor_from_class(cls, unreal.Vector(0, 0, 100)); a.set_actor_label(label)
try:
    sky.light_component.set_editor_property('real_time_capture', True)
except Exception:
    pass
'starter scene ready'
"""


def _code_result(r):
    parts = []
    if r.get("stdout"):
        parts.append(r["stdout"].rstrip())
    if r.get("result") is not None:
        v = r["result"]
        parts.append(v if isinstance(v, str) else json.dumps(v, ensure_ascii=False, indent=1))
    return "\n".join(parts) or "ok"


def do_exec(a, ctx):
    if not a.get("code"):
        raise ToolError("`code` is required")
    return _code_result(bridge("exec", code=a["code"], timeout=float(a.get("timeout_s") or 600)))


def do_actors(a, ctx):
    return bridge("actors", filter=a.get("filter"), limit=a.get("limit"))


def do_spawn(a, ctx):
    return bridge("spawn", asset=a.get("asset"), cls=a.get("cls"), location=a.get("location"), rotation=a.get("rotation"), scale=a.get("scale"), label=a.get("label"))


def do_transform(a, ctx):
    if not a.get("label"):
        raise ToolError("`label` is required")
    return bridge("transform", label=a["label"], location=a.get("location"), rotation=a.get("rotation"), scale=a.get("scale"))


def do_delete(a, ctx):
    if not a.get("label"):
        raise ToolError("`label` is required")
    return bridge("delete", label=a["label"])


def do_console(a, ctx):
    if not a.get("command"):
        raise ToolError("`command` is required")
    return bridge("console", command=a["command"])


def do_screenshot(a, ctx):
    """Viewport via HighResShot; falls back to a capture of the editor window."""
    if (a.get("what") or "viewport") == "viewport":
        r = bridge("screenshot", resolution=a.get("resolution"))
        path = r["path"]
        deadline = time.time() + 15
        last = -1
        while time.time() < deadline:
            if os.path.exists(path):
                sz = os.path.getsize(path)
                if sz > 0 and sz == last:
                    return [Image(path, max_bytes=a.get("max_bytes")), f"viewport: {path}"]
                last = sz
            time.sleep(0.3)
    res = ctx.nerve("ui", action="screenshot", target="window", title="Unreal Editor", max_bytes=a.get("max_bytes") or 60000)
    return res


def do_save(a, ctx):
    return bridge("save", timeout=600)


def do_open_level(a, ctx):
    if not a.get("level"):
        raise ToolError("`level` is required, e.g. /Game/Maps/Main")
    return bridge("open_level", level=a["level"])


def do_new_level(a, ctx):
    if not a.get("level"):
        raise ToolError("`level` is required, e.g. /Game/Maps/Arena")
    return bridge("new_level", level=a["level"])


def do_play(a, ctx):
    return bridge("play")


def do_stop(a, ctx):
    return bridge("stop")


def do_assets(a, ctx):
    return bridge("assets", path=a.get("path"), filter=a.get("filter"), limit=a.get("limit"))


def do_import(a, ctx):
    if not a.get("file"):
        raise ToolError("`file` is required (fbx, obj, gltf/glb, png, wav, ...)")
    return bridge("import", file=os.path.abspath(a["file"]), dest=a.get("dest"), timeout=1800)


def do_quit(a, ctx):
    if not alive():
        return "the editor is not running"
    if a.get("save"):
        bridge("save", timeout=600)
    return bridge("quit", timeout=10)


ACTIONS = {k[3:]: v for k, v in globals().items() if k.startswith("do_")}

V3 = {"type": "array", "items": {"type": "number"}}
SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": sorted(ACTIONS)},
        "project": {"type": "string", "description": "launch/install: project name (in Documents/Unreal Projects), folder or .uproject path; default = most recent."},
        "name": {"type": "string", "description": "new_project: project name."},
        "code": {"type": "string", "description": "exec: Python run in the editor (`unreal` module; `world`, `actors` = EditorActorSubsystem, `assets` = EditorAssetLibrary preloaded; the last expression or a `result` variable is returned)."},
        "asset": {"type": "string", "description": "spawn: asset path, e.g. /Engine/BasicShapes/Cube, /Game/BP_Enemy."},
        "cls": {"type": "string", "description": "spawn: actor class name in `unreal`, e.g. PointLight, CameraActor."},
        "label": {"type": "string", "description": "spawn: new label; transform/delete: actor label."},
        "location": {**V3, "description": "[x, y, z] in cm."},
        "rotation": {**V3, "description": "[pitch, yaw, roll] in degrees."},
        "scale": {**V3, "description": "[x, y, z]."},
        "command": {"type": "string", "description": "console: console command, e.g. stat fps, r.ScreenPercentage 50."},
        "level": {"type": "string", "description": "open_level/new_level/launch: level path like /Game/Maps/Main."},
        "path": {"type": "string", "description": "assets: content folder (default /Game)."},
        "filter": {"type": "string", "description": "actors/assets: substring filter."},
        "limit": {"type": "integer"},
        "file": {"type": "string", "description": "import: file on disk."},
        "dest": {"type": "string", "description": "import: content folder (default /Game/Imported)."},
        "what": {"type": "string", "enum": ["viewport", "window"], "description": "screenshot: viewport (HighResShot) or the whole editor window."},
        "resolution": {"type": "array", "items": {"type": "integer"}, "description": "screenshot: [w, h] (default 1280x720)."},
        "save": {"type": "boolean", "description": "quit: save everything first."},
        "engine": {"type": "string", "description": "new_project/launch: engine version like 5.8 (default newest)."},
        "launch": {"type": "boolean", "description": "new_project: open it right away (default true)."},
        "args": {"type": "array", "items": {"type": "string"}, "description": "launch: extra editor arguments."},
        "max_bytes": {"type": "integer"},
        "timeout_s": {"type": "number", "description": "launch: how long to wait for the editor (default 50 s; it keeps loading after that)."},
    },
    "required": ["action"],
}


@p.tool(
    schema=SCHEMA,
    description=(
        "Unreal Editor. Actions: status; projects; new_project {name} (empty project with a starter level, opens it); "
        "launch {project?} (opens the editor with the nerve bridge; first start may take minutes, poll status); install {project?}; "
        "exec {code} (editor Python: anything the unreal module can do); actors {filter?}; spawn {asset|cls, location?, rotation?, scale?, label?}; "
        "transform {label, location?, rotation?, scale?}; delete {label}; console {command}; screenshot {what?: viewport|window}; "
        "save; open_level/new_level {level}; play; stop; assets {path?, filter?}; import {file, dest?}; quit {save?}. Units: cm, degrees."
    ),
    annotations={"title": "Unreal Engine", "destructiveHint": True, "openWorldHint": False},
)
def unreal(ctx, **a):
    act = a.get("action") or "status"
    fn = ACTIONS.get(act)
    if fn is None:
        raise ToolError(f"unknown action `{act}`; valid: {', '.join(sorted(ACTIONS))}")
    return fn(a, ctx)


if __name__ == "__main__":
    p.run()
