"""nerve plugin SDK: write MCP tools for nerve in plain Python (stdlib only).

    from nerve_plugin import Plugin, Image, ToolError

    p = Plugin("hello")

    @p.tool
    def greet(name: str, times: int = 1) -> str:
        \"\"\"Say hello.

        Args:
            name: who to greet
            times: how many times
        \"\"\"
        return " ".join([f"hello {name}"] * times)

    p.run()

The input schema is built from the signature (str/int/float/bool/list/dict, Optional, Literal,
defaults) and the docstring's `Args:` section; pass `schema=` to override. A parameter named `ctx`
receives a Ctx (progress, cancellation, logging, calling nerve's own tools).
Return a str, a dict/list (sent as JSON), an Image, a list mixing those, or a raw MCP result
({"content": [...]}). Raise ToolError for a clean error message. print() goes to the plugin log
(<nerve data>/logs/plugins/<name>.log), never into the protocol stream.
"""

import base64
import inspect
import json
import os
import re
import subprocess
import sys
import threading
import traceback
import typing

__all__ = ["Plugin", "Ctx", "Image", "ToolError", "spawn_detached"]

try:
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8", newline="\n")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

_out = sys.stdout
sys.stdout = sys.stderr  # stray prints must never corrupt the protocol
_wlock = threading.Lock()


def _send(msg):
    data = json.dumps(msg, ensure_ascii=False, default=str)
    with _wlock:
        _out.write(data + "\n")
        _out.flush()


class ToolError(Exception):
    """Raise to return a clean error (isError) without a traceback."""


class Image:
    """An image result. With `path`, nerve loads the file and fits it into the client's budget."""

    def __init__(self, path=None, data=None, mime="image/png", max_bytes=None):
        self.path, self.data, self.mime, self.max_bytes = path, data, mime, max_bytes

    def content(self):
        if self.path is not None:
            c = {"type": "image", "path": os.path.abspath(str(self.path))}
            if self.max_bytes:
                c["max_bytes"] = int(self.max_bytes)
            return c
        return {"type": "image", "data": base64.b64encode(self.data).decode(), "mimeType": self.mime}


class Ctx:
    def __init__(self, plugin, req_id, token):
        self._plugin, self.request_id, self._token = plugin, req_id, token
        self.cancelled = threading.Event()
        self._n = 0

    @property
    def is_cancelled(self):
        return self.cancelled.is_set()

    def check(self):
        """Raise if the client cancelled the call."""
        if self.cancelled.is_set():
            raise ToolError("cancelled")

    def progress(self, message="", progress=None, total=None):
        if self._token is None:
            return
        self._n += 1
        p = {"progressToken": self._token, "progress": self._n if progress is None else progress, "message": str(message)}
        if total is not None:
            p["total"] = total
        _send({"jsonrpc": "2.0", "method": "notifications/progress", "params": p})

    def log(self, *args):
        print(*args, file=sys.stderr, flush=True)

    def nerve(self, tool, timeout=None, **arguments):
        """Call one of nerve's built-in tools (exec, fs, ui, browser, ...). Returns the MCP result dict."""
        return self._plugin.request("nerve/call", {"name": tool, "arguments": arguments}, timeout)

    def nerve_text(self, tool, **arguments):
        r = self.nerve(tool, **arguments)
        return "\n".join(c.get("text", "") for c in r.get("content", []) if c.get("type") == "text")


_JSON = {str: "string", int: "integer", float: "number", bool: "boolean", list: "array", dict: "object", tuple: "array"}


def _type_schema(t):
    if t is inspect.Parameter.empty or t is typing.Any:
        return {}
    origin, args = typing.get_origin(t), typing.get_args(t)
    if origin is typing.Union or (origin is not None and getattr(origin, "__name__", "") == "UnionType") or type(t).__name__ == "UnionType":
        nn = [a for a in args if a is not type(None)]
        return _type_schema(nn[0]) if len(nn) == 1 else {}
    if origin is typing.Literal:
        vals = list(args)
        s = {"enum": vals}
        if vals and type(vals[0]) in _JSON:
            s["type"] = _JSON[type(vals[0])]
        return s
    if origin in (list, tuple, set, frozenset):
        s = {"type": "array"}
        if args and args[0] is not Ellipsis:
            s["items"] = _type_schema(args[0])
        return s
    if origin is dict:
        return {"type": "object"}
    if t in _JSON:
        return {"type": _JSON[t]}
    return {}


def _doc_parts(doc):
    """(description, {param: description}) from a Google-style docstring."""
    doc = inspect.cleandoc(doc or "")
    m = re.search(r"^\s*(Args|Arguments|Parameters|Params):\s*$", doc, re.M)
    if not m:
        return doc.strip(), {}
    desc, rest = doc[: m.start()].strip(), doc[m.end():]
    params, cur = {}, None
    for line in rest.splitlines():
        if not line.strip():
            continue
        mm = re.match(r"^\s{0,8}(\*{0,2}\w+)\s*(\([^)]*\))?\s*:\s*(.*)$", line)
        if mm and (cur is None or len(line) - len(line.lstrip()) <= params.get("__indent__", 99)):
            cur = mm.group(1).lstrip("*")
            params["__indent__"] = len(line) - len(line.lstrip())
            params[cur] = mm.group(3).strip()
        elif cur and line.startswith(" "):
            params[cur] += " " + line.strip()
        else:
            break
    params.pop("__indent__", None)
    return desc, params


def schema_for(fn):
    sig = inspect.signature(fn)
    try:
        hints = typing.get_type_hints(fn)
    except Exception:
        hints = {}
    _, pdocs = _doc_parts(fn.__doc__)
    props, req = {}, []
    for name, p in sig.parameters.items():
        if name in ("ctx", "self") or p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            continue
        s = _type_schema(hints.get(name, p.annotation))
        if p.default is not p.empty:
            if p.default is not None and isinstance(p.default, (str, int, float, bool, list, dict)):
                s["default"] = p.default
        else:
            req.append(name)
        if name in pdocs:
            s["description"] = pdocs[name]
        props[name] = s
    out = {"type": "object", "properties": props}
    if req:
        out["required"] = req
    return out


def _content(x):
    if x is None:
        return []
    if isinstance(x, Image):
        return [x.content()]
    if isinstance(x, (bytes, bytearray)):
        return [{"type": "text", "text": x.decode("utf-8", "replace")}]
    if isinstance(x, str):
        return [{"type": "text", "text": x}]
    return [{"type": "text", "text": json.dumps(x, ensure_ascii=False, indent=1, default=str)}]


def _result(r):
    if isinstance(r, dict) and isinstance(r.get("content"), list):
        r.setdefault("isError", False)
        return r
    items = r if isinstance(r, list) and any(isinstance(i, Image) for i in r) else [r]
    content = [c for i in items for c in _content(i)]
    return {"content": content or [{"type": "text", "text": "ok"}], "isError": False}


class _Tool:
    def __init__(self, fn, name, description, schema, annotations):
        self.fn, self.name = fn, name
        desc, _ = _doc_parts(fn.__doc__)
        self.description = description or desc or name
        self.schema = schema or schema_for(fn)
        self.annotations = annotations
        sig = inspect.signature(fn)
        self.params = set(sig.parameters)
        self.var_kw = any(p.kind == p.VAR_KEYWORD for p in sig.parameters.values())

    def json(self):
        d = {"name": self.name, "description": self.description, "inputSchema": self.schema}
        if self.annotations:
            d["annotations"] = self.annotations
        return d


class Plugin:
    def __init__(self, name=None, version="0.1.0", instructions=None):
        self.name = name or os.environ.get("NERVE_PLUGIN") or "plugin"
        self.version, self.instructions = version, instructions
        self.tools = {}
        self._calls = {}
        self._waiters = {}
        self._next = 0
        self._lock = threading.Lock()

    def tool(self, fn=None, *, name=None, description=None, schema=None, annotations=None):
        """Decorator registering a tool. Usable as @p.tool or @p.tool(name=..., schema=...)."""

        def wrap(f):
            t = _Tool(f, name or f.__name__, description, schema, annotations)
            self.tools[t.name] = t
            return f

        return wrap(fn) if fn is not None else wrap

    # -- plugin -> nerve requests
    def request(self, method, params, timeout=None):
        with self._lock:
            self._next += 1
            rid = f"n{self._next}"
            ev = threading.Event()
            self._waiters[rid] = [ev, None]
        _send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        if not ev.wait(timeout):
            self._waiters.pop(rid, None)
            raise ToolError(f"nerve did not answer {method} in {timeout}s")
        msg = self._waiters.pop(rid)[1]
        if "error" in msg:
            raise ToolError(msg["error"].get("message", str(msg["error"])))
        return msg.get("result")

    # -- serving
    def run(self):
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except Exception:
                print("bad json from nerve:", line[:200], file=sys.stderr)
                continue
            try:
                self._handle(msg)
            except Exception:
                traceback.print_exc()

    def _reply(self, rid, result=None, error=None):
        m = {"jsonrpc": "2.0", "id": rid}
        if error is not None:
            m["error"] = error
        else:
            m["result"] = result
        _send(m)

    def _handle(self, msg):
        method, rid = msg.get("method"), msg.get("id")
        if method is None:
            if rid in self._waiters:
                w = self._waiters[rid]
                w[1] = msg
                w[0].set()
            return
        params = msg.get("params") or {}
        if rid is None:
            if method == "notifications/cancelled":
                c = self._calls.get(params.get("requestId"))
                if c:
                    c.cancelled.set()
            return
        if method == "initialize":
            r = {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": self.name, "version": self.version},
            }
            if self.instructions:
                r["instructions"] = self.instructions
            self._reply(rid, r)
        elif method == "tools/list":
            self._reply(rid, {"tools": [t.json() for t in self.tools.values()]})
        elif method == "tools/call":
            threading.Thread(target=self._call, args=(rid, params), daemon=True).start()
        elif method == "ping":
            self._reply(rid, {})
        else:
            self._reply(rid, error={"code": -32601, "message": f"method not found: {method}"})

    def _call(self, rid, params):
        name = params.get("name", "")
        args = params.get("arguments") or {}
        t = self.tools.get(name)
        if t is None:
            self._reply(rid, {"content": [{"type": "text", "text": f"unknown tool {name}"}], "isError": True})
            return
        ctx = Ctx(self, rid, (params.get("_meta") or {}).get("progressToken"))
        self._calls[rid] = ctx
        try:
            kw = dict(args) if t.var_kw else {k: v for k, v in args.items() if k in t.params}
            unknown = [k for k in args if k not in t.params] if not t.var_kw else []
            if "ctx" in t.params:
                kw["ctx"] = ctx
            res = _result(t.fn(**kw))
            if unknown:
                res["content"].append({"type": "text", "text": f"(ignored unknown argument(s): {', '.join(unknown)})"})
        except ToolError as e:
            res = {"content": [{"type": "text", "text": str(e)}], "isError": True}
        except TypeError as e:
            res = {"content": [{"type": "text", "text": f"bad arguments: {e}"}], "isError": True}
        except Exception as e:
            tb = traceback.format_exc()
            print(tb, file=sys.stderr)
            res = {"content": [{"type": "text", "text": f"{type(e).__name__}: {e}\n{tb[-3000:]}"}], "isError": True}
        finally:
            self._calls.pop(rid, None)
        self._reply(rid, res)


def spawn_detached(argv, cwd=None, env=None, log=None, hidden=False):
    """Start a program that must outlive the plugin (and nerve restarts), e.g. Blender or an editor."""
    out = open(log, "ab") if log else subprocess.DEVNULL
    kw = dict(cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT if log else subprocess.DEVNULL, close_fds=True)
    if os.name == "nt":
        base = 0x00000200 | (0x08000000 if hidden else 0x00000008)  # NEW_PROCESS_GROUP | NO_WINDOW/DETACHED
        try:
            return subprocess.Popen(argv, creationflags=base | 0x01000000, **kw)  # BREAKAWAY_FROM_JOB
        except OSError:
            return subprocess.Popen(argv, creationflags=base, **kw)
    return subprocess.Popen(argv, start_new_session=True, **kw)
