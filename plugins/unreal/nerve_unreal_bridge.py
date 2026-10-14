"""nerve bridge for the Unreal Editor (Python Editor Script Plugin).

Loaded at editor start by init_unreal.py (installed by the nerve `unreal` plugin into
Documents/UnrealEngine/Python). Listens on 127.0.0.1:9877; one JSON object per line in, one out.
Commands run on the game thread from a Slate post-tick callback.
"""

import ast
import contextlib
import io
import json
import os
import queue
import socket
import tempfile
import threading
import time
import traceback

import unreal

PORT = int(os.environ.get("NERVE_UNREAL_PORT", "9877"))
_q = queue.Queue()
_ns = {}
_state = {"sock": None, "tick": None}


def _jsonable(v, depth=0):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if depth > 3:
        return str(v)
    if isinstance(v, dict):
        return {str(k): _jsonable(x, depth + 1) for k, x in list(v.items())[:500]}
    if isinstance(v, (list, tuple, set)) or type(v).__name__ == "Array":
        return [_jsonable(x, depth + 1) for x in list(v)[:500]]
    if isinstance(v, unreal.Vector):
        return [round(v.x, 3), round(v.y, 3), round(v.z, 3)]
    if isinstance(v, unreal.Rotator):
        return {"pitch": round(v.pitch, 2), "yaw": round(v.yaw, 2), "roll": round(v.roll, 2)}
    if isinstance(v, unreal.Actor):
        return _actor(v)
    if isinstance(v, unreal.Object):
        try:
            return v.get_path_name()
        except Exception:
            return str(v)
    return str(v)


def _sub(cls):
    return unreal.get_editor_subsystem(cls)


def _world():
    try:
        return _sub(unreal.UnrealEditorSubsystem).get_editor_world()
    except Exception:
        return unreal.EditorLevelLibrary.get_editor_world()


def _actors():
    return list(_sub(unreal.EditorActorSubsystem).get_all_level_actors())


def _actor(a):
    loc, rot, sc = a.get_actor_location(), a.get_actor_rotation(), a.get_actor_scale3d()
    return {
        "label": a.get_actor_label(),
        "class": a.get_class().get_name(),
        "loc": [round(loc.x, 2), round(loc.y, 2), round(loc.z, 2)],
        "rot": [round(rot.pitch, 2), round(rot.yaw, 2), round(rot.roll, 2)],
        "scale": [round(sc.x, 3), round(sc.y, 3), round(sc.z, 3)],
        "path": a.get_path_name(),
    }


def _find_actor(label):
    for a in _actors():
        if a.get_actor_label() == label or a.get_name() == label:
            return a
    raise RuntimeError(f"no actor labelled {label!r}")


def _vec(v, default=(0, 0, 0)):
    v = v or default
    return unreal.Vector(float(v[0]), float(v[1]), float(v[2]))


def _rot(v):
    v = v or (0, 0, 0)
    return unreal.Rotator(roll=float(v[2]), pitch=float(v[0]), yaw=float(v[1]))


def run_code(code):
    tree = ast.parse(code, "<nerve>", "exec")
    last = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last = ast.Expression(tree.body.pop().value)
    if not _ns:
        _ns.update({"unreal": unreal, "__name__": "__nerve__"})
    _ns["world"] = _world()
    _ns["actors"] = _sub(unreal.EditorActorSubsystem)
    _ns["assets"] = unreal.EditorAssetLibrary
    _ns.pop("result", None)
    buf = io.StringIO()
    value = None
    with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
        exec(compile(tree, "<nerve>", "exec"), _ns)
        if last is not None:
            value = eval(compile(last, "<nerve>", "eval"), _ns)
    if value is None and "result" in _ns:
        value = _ns["result"]
    return {"result": _jsonable(value), "stdout": buf.getvalue()[-20000:]}


# ------------------------------------------------------------------ ops

def op_ping(r):
    w = _world()
    return {
        "engine": unreal.SystemLibrary.get_engine_version(),
        "project": unreal.Paths.get_project_file_path(),
        "level": w.get_name() if w else None,
        "level_path": w.get_path_name() if w else None,
        "actors": len(_actors()),
        "pid": os.getpid(),
        "playing": bool(unreal.EditorLevelLibrary.get_game_world()) if hasattr(unreal.EditorLevelLibrary, "get_game_world") else None,
    }


def op_exec(r):
    return run_code(r.get("code", ""))


def op_actors(r):
    flt = (r.get("filter") or "").lower()
    out = []
    for a in _actors():
        if flt and flt not in a.get_actor_label().lower() and flt not in a.get_class().get_name().lower():
            continue
        out.append(_actor(a))
    limit = int(r.get("limit") or 300)
    return {"count": len(out), "actors": out[:limit], "truncated": max(0, len(out) - limit)}


def op_spawn(r):
    what = r.get("asset") or r.get("cls") or "/Engine/BasicShapes/Cube"
    eas = _sub(unreal.EditorActorSubsystem)
    loc, rot = _vec(r.get("location")), _rot(r.get("rotation"))
    if what.startswith("/"):
        asset = unreal.EditorAssetLibrary.load_asset(what)
        if asset is None:
            raise RuntimeError(f"asset not found: {what}")
        if isinstance(asset, unreal.Blueprint):
            actor = eas.spawn_actor_from_class(asset.generated_class(), loc, rot)
        else:
            actor = eas.spawn_actor_from_object(asset, loc, rot)
    else:
        cls = getattr(unreal, what, None)
        if cls is None:
            raise RuntimeError(f"unknown class unreal.{what} (or pass an asset path like /Engine/BasicShapes/Cube)")
        actor = eas.spawn_actor_from_class(cls, loc, rot)
    if actor is None:
        raise RuntimeError("spawn failed")
    if r.get("scale"):
        actor.set_actor_scale3d(_vec(r["scale"]))
    if r.get("label"):
        actor.set_actor_label(r["label"])
    return _actor(actor)


def op_transform(r):
    a = _find_actor(r["label"])
    if r.get("location") is not None:
        a.set_actor_location(_vec(r["location"]), False, False)
    if r.get("rotation") is not None:
        a.set_actor_rotation(_rot(r["rotation"]), False)
    if r.get("scale") is not None:
        a.set_actor_scale3d(_vec(r["scale"]))
    return _actor(a)


def op_delete(r):
    a = _find_actor(r["label"])
    _sub(unreal.EditorActorSubsystem).destroy_actor(a)
    return {"deleted": r["label"]}


def op_console(r):
    unreal.SystemLibrary.execute_console_command(_world(), r["command"])
    return {"ran": r["command"]}


def op_screenshot(r):
    d = os.environ.get("NERVE_UNREAL_OUT") or os.path.join(tempfile.gettempdir(), "nerve-unreal")
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, f"viewport-{time.strftime('%Y%m%d-%H%M%S')}.png")
    res = r.get("resolution") or [1280, 720]
    unreal.AutomationLibrary.take_high_res_screenshot(int(res[0]), int(res[1]), path)
    return {"path": path, "pending": True}


def op_save(r):
    ok = unreal.EditorLoadingAndSavingUtils.save_dirty_packages(True, True)
    return {"saved": bool(ok)}


def op_open_level(r):
    ok = _sub(unreal.LevelEditorSubsystem).load_level(r["level"])
    if not ok:
        raise RuntimeError(f"could not open level {r['level']}")
    return op_ping(r)


def op_new_level(r):
    path = r["level"]
    ok = _sub(unreal.LevelEditorSubsystem).new_level(path)
    if not ok:
        raise RuntimeError(f"could not create level {path}")
    return op_ping(r)


def op_play(r):
    _sub(unreal.LevelEditorSubsystem).editor_request_begin_play()
    return {"play": "requested"}


def op_stop(r):
    _sub(unreal.LevelEditorSubsystem).editor_request_end_play()
    return {"stop": "requested"}


def op_assets(r):
    path = r.get("path") or "/Game"
    flt = (r.get("filter") or "").lower()
    items = [a for a in unreal.EditorAssetLibrary.list_assets(path, recursive=True, include_folder=False) if not flt or flt in a.lower()]
    limit = int(r.get("limit") or 300)
    return {"count": len(items), "assets": [str(a) for a in items[:limit]], "truncated": max(0, len(items) - limit)}


def op_import(r):
    t = unreal.AssetImportTask()
    t.filename = r["file"]
    t.destination_path = r.get("dest") or "/Game/Imported"
    t.automated = True
    t.save = True
    t.replace_existing = True
    unreal.AssetToolsHelpers.get_asset_tools().import_asset_tasks([t])
    return {"imported": [str(p) for p in t.imported_object_paths]}


def op_quit(r):
    def later(dt):
        unreal.unregister_slate_post_tick_callback(_state["quit_cb"])
        unreal.SystemLibrary.quit_editor()

    _state["quit_cb"] = unreal.register_slate_post_tick_callback(later)
    return {"quitting": True}


OPS = {k[3:]: v for k, v in globals().items() if k.startswith("op_")}


def _handle(req):
    fn = OPS.get(req.get("op", ""))
    if fn is None:
        return {"ok": False, "error": f"unknown op {req.get('op')}"}
    try:
        return {"ok": True, "data": fn(req)}
    except Exception as e:
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "trace": traceback.format_exc()[-4000:]}


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
                rep = {"ok": False, "error": "timed out waiting for the editor's game thread"}
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


def _accept(sock):
    while True:
        try:
            conn, _ = sock.accept()
        except OSError:
            break
        threading.Thread(target=_serve_conn, args=(conn,), daemon=True).start()


def _tick(dt):
    t0 = time.time()
    while time.time() - t0 < 0.03:
        try:
            req, box = _q.get_nowait()
        except queue.Empty:
            break
        box.put(_handle(req))


def start():
    if _state["sock"] is not None:
        return False
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
        s.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
    try:
        s.bind(("127.0.0.1", PORT))
    except OSError as e:
        unreal.log_warning(f"[nerve bridge] port {PORT} busy ({e}); another editor already listens")
        s.close()
        return False
    s.listen(8)
    _state["sock"] = s
    threading.Thread(target=_accept, args=(s,), daemon=True).start()
    _state["tick"] = unreal.register_slate_post_tick_callback(_tick)
    unreal.log(f"[nerve bridge] listening on 127.0.0.1:{PORT}")
    return True


def stop():
    if _state["tick"] is not None:
        unreal.unregister_slate_post_tick_callback(_state["tick"])
        _state["tick"] = None
    if _state["sock"] is not None:
        _state["sock"].close()
        _state["sock"] = None
