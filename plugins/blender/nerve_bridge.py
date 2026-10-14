"""nerve bridge for Blender: lets the nerve `blender` plugin drive this Blender over 127.0.0.1.

Works three ways:
  * blender --python nerve_bridge.py            (GUI; nerve's `launch` does this)
  * blender -b [file] --python nerve_bridge.py  (headless session, NERVE_BLENDER_SERVE=1)
  * installed as an add-on (Preferences > Add-ons > nerve bridge): every Blender you open listens.

Protocol: one JSON object per line -> one JSON reply per line. Commands run on Blender's main thread.
"""

bl_info = {
    "name": "nerve bridge",
    "author": "nerve",
    "version": (1, 0, 0),
    "blender": (3, 0, 0),
    "location": "runs in the background",
    "description": "Lets nerve (MCP server) control this Blender via 127.0.0.1",
    "category": "System",
}

import ast
import contextlib
import io
import json
import math
import os
import queue
import socket
import sys
import tempfile
import threading
import time
import traceback

import bpy

PORT = int(os.environ.get("NERVE_BLENDER_PORT", "9886"))
_q = queue.Queue()
_ns = {}
_KEY = "nerve_bridge"
_KIT = {"path": os.environ.get("NERVE_BLENDER_KIT"), "mtime": None, "mod": None}


# ------------------------------------------------------------------ helpers

def _jsonable(v, depth=0):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if depth > 3:
        return repr(v)
    if isinstance(v, dict):
        return {str(k): _jsonable(x, depth + 1) for k, x in list(v.items())[:500]}
    if isinstance(v, (list, tuple, set)):
        return [_jsonable(x, depth + 1) for x in list(v)[:500]]
    if hasattr(v, "to_tuple"):
        try:
            return [round(x, 5) for x in v.to_tuple()]
        except Exception:
            pass
    if hasattr(v, "__len__") and hasattr(v, "__getitem__") and not hasattr(v, "bl_rna"):
        try:
            return [_jsonable(x, depth + 1) for x in list(v)[:500]]
        except Exception:
            pass
    return repr(v)


def _view3d():
    """(window, area, region) of the largest 3D viewport, or None."""
    best = None
    wm = bpy.context.window_manager
    for win in (wm.windows if wm else []):
        for area in win.screen.areas:
            if area.type == "VIEW_3D":
                reg = next((r for r in area.regions if r.type == "WINDOW"), None)
                if reg and (best is None or area.width * area.height > best[1].width * best[1].height):
                    best = (win, area, reg)
    return best


def _override():
    v = _view3d()
    if v:
        win, area, reg = v
        return bpy.context.temp_override(window=win, screen=win.screen, area=area, region=reg)
    wm = bpy.context.window_manager
    if wm and wm.windows:
        win = wm.windows[0]
        return bpy.context.temp_override(window=win, screen=win.screen)
    return contextlib.nullcontext()


def _out(name, ext="png"):
    d = os.environ.get("NERVE_BLENDER_OUT") or os.path.join(tempfile.gettempdir(), "nerve-blender")
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, f"{name}-{time.strftime('%Y%m%d-%H%M%S')}-{int(time.time() * 1000) % 1000:03d}.{ext}")


def _kit_path():
    cands = [_KIT["path"], os.path.join(os.path.dirname(os.path.abspath(__file__)), "nerve_kit.py")]
    if os.environ.get("LOCALAPPDATA"):
        cands.append(os.path.join(os.environ["LOCALAPPDATA"], "nerve", "plugins", "blender", "nerve_kit.py"))
    return next((p for p in cands if p and os.path.isfile(p)), None)


def _kit():
    """The nerve kit module (character modeling helpers), reloaded when its file changes."""
    import importlib.util

    path = _kit_path()
    if path is None:
        return None
    mtime = os.path.getmtime(path)
    if _KIT["mod"] is None or _KIT["mtime"] != mtime or _KIT.get("loaded") != path:
        spec = importlib.util.spec_from_file_location("nerve_kit", path)
        mod = importlib.util.module_from_spec(spec)
        sys.modules["nerve_kit"] = mod
        spec.loader.exec_module(mod)
        _KIT.update(mod=mod, mtime=mtime, loaded=path)
    return _KIT["mod"]


def run_code(code):
    tree = ast.parse(code, "<nerve>", "exec")
    last = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last = ast.Expression(tree.body.pop().value)
    if not _ns:
        import mathutils

        _ns.update({"bpy": bpy, "mathutils": mathutils, "Vector": mathutils.Vector, "Matrix": mathutils.Matrix, "Euler": mathutils.Euler, "math": math, "__name__": "__nerve__"})
    _ns["C"], _ns["D"] = bpy.context, bpy.data
    _ns["kit"] = _kit()
    _ns.pop("result", None)
    buf = io.StringIO()
    value = None
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf), _override():
        exec(compile(tree, "<nerve>", "exec"), _ns)
        if last is not None:
            value = eval(compile(last, "<nerve>", "eval"), _ns)
    if value is None and "result" in _ns:
        value = _ns["result"]
    return {"result": _jsonable(value), "stdout": buf.getvalue()[-20000:]}


# ------------------------------------------------------------------ ops

def op_ping(r):
    sc = bpy.context.scene
    return {
        "version": bpy.app.version_string,
        "pid": os.getpid(),
        "background": bpy.app.background,
        "file": bpy.data.filepath,
        "dirty": bpy.data.is_dirty,
        "scene": sc.name if sc else None,
        "objects": len(bpy.data.objects),
        "executable": bpy.app.binary_path,
    }


def op_exec(r):
    return run_code(r.get("code", ""))


def _obj(o):
    d = {
        "name": o.name,
        "type": o.type,
        "loc": [round(x, 3) for x in o.location],
        "rot": [round(math.degrees(x), 1) for x in o.rotation_euler],
        "scale": [round(x, 3) for x in o.scale],
        "dims": [round(x, 3) for x in o.dimensions],
    }
    if o.parent:
        d["parent"] = o.parent.name
    if o.data is not None and hasattr(o.data, "materials"):
        mats = [m.name for m in o.data.materials if m]
        if mats:
            d["materials"] = mats
    if o.type == "MESH" and o.data:
        d["verts"] = len(o.data.vertices)
        d["faces"] = len(o.data.polygons)
    if o.type == "LIGHT":
        d["light"] = o.data.type
        d["energy"] = round(o.data.energy, 2)
    if o.modifiers:
        d["modifiers"] = [m.type for m in o.modifiers]
    if o.hide_get() if hasattr(o, "hide_get") else False:
        d["hidden"] = True
    try:
        if o.select_get():
            d["selected"] = True
    except Exception:
        pass
    return d


def op_scene(r):
    sc = bpy.context.scene
    limit = int(r.get("limit", 300))
    flt = (r.get("filter") or "").lower()
    objs = [o for o in sc.objects if not flt or flt in o.name.lower() or flt in o.type.lower()]
    rd = sc.render
    active = bpy.context.view_layer.objects.active if bpy.context.view_layer else None
    return {
        "file": bpy.data.filepath or "(unsaved)",
        "scene": sc.name,
        "engine": rd.engine,
        "resolution": [rd.resolution_x, rd.resolution_y, rd.resolution_percentage],
        "frames": [sc.frame_start, sc.frame_end, sc.frame_current],
        "fps": rd.fps,
        "camera": sc.camera.name if sc.camera else None,
        "active": active.name if active else None,
        "collections": [c.name for c in bpy.data.collections][:100],
        "counts": {"objects": len(sc.objects), "meshes": len(bpy.data.meshes), "materials": len(bpy.data.materials), "images": len(bpy.data.images)},
        "objects": [_obj(o) for o in objs[:limit]],
        "truncated": max(0, len(objs) - limit),
    }


def op_screenshot(r):
    if bpy.app.background:
        raise RuntimeError("headless Blender has no viewport: use action render")
    v = _view3d()
    path = _out("viewport")
    if v is None:
        with _override():
            bpy.ops.screen.screenshot(filepath=path)
        return {"path": path, "what": "window"}
    win, area, reg = v
    space = area.spaces.active
    old_shading = None
    with bpy.context.temp_override(window=win, screen=win.screen, area=area, region=reg):
        if r.get("shading"):
            old_shading = space.shading.type
            space.shading.type = r["shading"].upper()
        view = (r.get("view") or "").lower()
        if view == "camera" and bpy.context.scene.camera:
            if space.region_3d.view_perspective != "CAMERA":
                bpy.ops.view3d.view_camera()
        elif view == "all":
            bpy.ops.view3d.view_all(center=False)
        elif view == "selected":
            bpy.ops.view3d.view_selected()
        elif view in ("front", "back", "left", "right", "top", "bottom"):
            bpy.ops.view3d.view_axis(type=view.upper())
            bpy.ops.view3d.view_all(center=False)
        bpy.ops.wm.redraw_timer(type="DRAW_WIN_SWAP", iterations=1)
        bpy.ops.screen.screenshot_area(filepath=path)
        if old_shading:
            space.shading.type = old_shading
    return {"path": path, "what": "viewport", "size": [area.width, area.height]}


def _set_engine(sc, engine):
    e = engine.upper()
    names = {"EEVEE": ["BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"], "CYCLES": ["CYCLES"], "WORKBENCH": ["BLENDER_WORKBENCH"]}
    for n in names.get(e, [e]):
        try:
            sc.render.engine = n
            return
        except TypeError:
            continue
    raise RuntimeError(f"unknown render engine {engine}")


def op_render(r):
    sc = bpy.context.scene
    if sc.camera is None:
        raise RuntimeError("the scene has no camera (add one, e.g. exec: bpy.ops.object.camera_add())")
    rd = sc.render
    saved = (rd.engine, rd.resolution_x, rd.resolution_y, rd.resolution_percentage, rd.filepath, rd.image_settings.file_format)
    try:
        if r.get("engine"):
            _set_engine(sc, r["engine"])
        if r.get("resolution"):
            rd.resolution_x, rd.resolution_y = int(r["resolution"][0]), int(r["resolution"][1])
            rd.resolution_percentage = 100
        if r.get("samples"):
            n = int(r["samples"])
            if rd.engine == "CYCLES":
                sc.cycles.samples = n
            elif hasattr(sc, "eevee"):
                sc.eevee.taa_render_samples = n
        if r.get("frame") is not None:
            sc.frame_set(int(r["frame"]))
        path = r.get("output") or _out("render")
        rd.filepath = path
        rd.image_settings.file_format = "PNG"
        t0 = time.time()
        bpy.ops.render.render(write_still=True)
        return {"path": bpy.path.abspath(path), "seconds": round(time.time() - t0, 2), "engine": rd.engine, "size": [rd.resolution_x, rd.resolution_y]}
    finally:
        if not r.get("keep_settings"):
            rd.engine, rd.resolution_x, rd.resolution_y, rd.resolution_percentage, rd.filepath, rd.image_settings.file_format = saved


def _need_kit():
    k = _kit()
    if k is None:
        raise RuntimeError("nerve_kit.py not found next to the bridge or in the nerve plugins folder")
    return k


def op_sheet(r):
    keys = ("objects", "views", "mode", "size", "focus", "lens", "elevation", "cols", "out", "samples")
    kw = {k: r[k] for k in keys if r.get(k) is not None}
    with _override():
        return _need_kit().sheet(**kw)


def op_topo(r):
    return _need_kit().topo(r.get("object"))


def op_kit(r):
    return _need_kit().help()


def op_open(r):
    path = r["file"]
    if not os.path.exists(path):
        raise RuntimeError(f"no such file: {path}")
    with _override():
        bpy.ops.wm.open_mainfile(filepath=path)
    return op_ping(r)


def op_save(r):
    path = r.get("file") or bpy.data.filepath
    if not path:
        raise RuntimeError("the file was never saved: pass `file`")
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with _override():
        bpy.ops.wm.save_as_mainfile(filepath=path, copy=bool(r.get("copy")))
    return {"saved": path}


def op_new(r):
    with _override():
        bpy.ops.wm.read_homefile(use_empty=bool(r.get("empty")))
    return op_ping(r)


_IMPORT = {
    ".fbx": ("import_scene.fbx", {}),
    ".obj": ("wm.obj_import", {}),
    ".glb": ("import_scene.gltf", {}),
    ".gltf": ("import_scene.gltf", {}),
    ".stl": ("wm.stl_import", {}),
    ".ply": ("wm.ply_import", {}),
    ".usd": ("wm.usd_import", {}),
    ".usda": ("wm.usd_import", {}),
    ".usdc": ("wm.usd_import", {}),
    ".usdz": ("wm.usd_import", {}),
    ".abc": ("wm.alembic_import", {}),
    ".svg": ("import_curve.svg", {}),
}
_EXPORT = {
    ".fbx": ("export_scene.fbx", "use_selection"),
    ".obj": ("wm.obj_export", "export_selected_objects"),
    ".glb": ("export_scene.gltf", "use_selection"),
    ".gltf": ("export_scene.gltf", "use_selection"),
    ".stl": ("wm.stl_export", "export_selected_objects"),
    ".ply": ("wm.ply_export", "export_selected_objects"),
    ".usd": ("wm.usd_export", "selected_objects_only"),
    ".usdc": ("wm.usd_export", "selected_objects_only"),
    ".usda": ("wm.usd_export", "selected_objects_only"),
    ".abc": ("wm.alembic_export", "selected"),
}


def _op(path):
    mod, name = path.split(".")
    return getattr(getattr(bpy.ops, mod), name)


def op_import(r):
    path = r["file"]
    ext = os.path.splitext(path)[1].lower()
    if ext == ".blend":
        with bpy.data.libraries.load(path, link=False) as (src, dst):
            dst.objects = list(src.objects)
        for o in dst.objects:
            if o is not None:
                bpy.context.scene.collection.objects.link(o)
        return {"imported": [o.name for o in dst.objects if o]}
    if ext not in _IMPORT:
        raise RuntimeError(f"don't know how to import {ext} (supported: {', '.join(sorted(_IMPORT))}, .blend)")
    before = set(bpy.data.objects.keys())
    with _override():
        _op(_IMPORT[ext][0])(filepath=path)
    new = [n for n in bpy.data.objects.keys() if n not in before]
    return {"imported": new}


def op_export(r):
    path = r["file"]
    ext = os.path.splitext(path)[1].lower()
    if ext not in _EXPORT:
        raise RuntimeError(f"don't know how to export {ext} (supported: {', '.join(sorted(_EXPORT))})")
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    op, sel = _EXPORT[ext]
    kw = {"filepath": path}
    if r.get("selected"):
        kw[sel] = True
    if ext in (".glb", ".gltf"):
        kw["export_format"] = "GLB" if ext == ".glb" else "GLTF_SEPARATE"
    with _override():
        _op(op)(**kw)
    return {"exported": path, "bytes": os.path.getsize(path) if os.path.exists(path) else None}


_quit = threading.Event()


def op_quit(r):
    _quit.set()
    if not bpy.app.background:
        def later():
            with _override():
                bpy.ops.wm.quit_blender()
        bpy.app.timers.register(later, first_interval=0.3)
    return {"quitting": True}


OPS = {k[3:]: v for k, v in globals().items() if k.startswith("op_")}


def _handle(req):
    op = req.get("op", "")
    if req.get("kit"):
        _KIT["path"] = req["kit"]
    fn = OPS.get(op)
    if fn is None:
        return {"ok": False, "error": f"unknown op {op}"}
    try:
        return {"ok": True, "data": fn(req)}
    except Exception as e:
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "trace": traceback.format_exc()[-4000:]}


# ------------------------------------------------------------------ server

def _serve_conn(conn):
    f = conn.makefile("rwb")
    try:
        for line in f:
            try:
                req = json.loads(line)
            except Exception:
                continue
            box = queue.Queue(1)
            _q.put((req, box))
            try:
                rep = box.get(timeout=float(req.get("timeout", 3600)))
            except queue.Empty:
                rep = {"ok": False, "error": "timed out waiting for Blender's main thread"}
            rep["id"] = req.get("id")
            f.write((json.dumps(rep, default=str) + "\n").encode())
            f.flush()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def _accept_loop(sock):
    while True:
        try:
            conn, _ = sock.accept()
        except OSError:
            break
        threading.Thread(target=_serve_conn, args=(conn,), daemon=True).start()


def _pump():
    t0 = time.time()
    while time.time() - t0 < 0.05:
        try:
            req, box = _q.get_nowait()
        except queue.Empty:
            break
        box.put(_handle(req))
    return 0.05


def start():
    ns = bpy.app.driver_namespace
    if ns.get(_KEY):
        return False
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
        s.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
    try:
        s.bind(("127.0.0.1", PORT))
    except OSError as e:
        print(f"[nerve bridge] port {PORT} busy ({e}); another Blender already listens")
        s.close()
        return False
    s.listen(8)
    ns[_KEY] = s
    threading.Thread(target=_accept_loop, args=(s,), daemon=True).start()
    if not bpy.app.background:
        bpy.app.timers.register(_pump, first_interval=0.1, persistent=True)
    print(f"[nerve bridge] listening on 127.0.0.1:{PORT}")
    return True


def serve_forever():
    """Headless mode: run commands on the main thread until `quit`."""
    while not _quit.is_set():
        try:
            req, box = _q.get(timeout=0.2)
        except queue.Empty:
            continue
        box.put(_handle(req))


def stop():
    s = bpy.app.driver_namespace.pop(_KEY, None)
    if s:
        try:
            s.close()
        except Exception:
            pass
    if bpy.app.timers.is_registered(_pump):
        bpy.app.timers.unregister(_pump)


# add-on entry points
def register():
    if bpy.app.background and os.environ.get("NERVE_BLENDER_SERVE") != "1":
        return
    start()


def unregister():
    stop()


if __name__ == "__main__":
    started = start()
    if bpy.app.background and os.environ.get("NERVE_BLENDER_SERVE") == "1" and started:
        serve_forever()
