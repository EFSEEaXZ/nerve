# nerve plugins

Every folder here with a `plugin.toml` is a plugin: a small program that speaks MCP over
stdin/stdout (one JSON-RPC message per line). nerve starts it, exposes its tools next to the
built-in ones and **hot-reloads** it whenever a file in the folder changes. No nerve restart needed.

```
plugins/
  _sdk/nerve_plugin.py     Python SDK (on PYTHONPATH of every plugin)
  hello/
    plugin.toml
    server.py
```

## plugin.toml

```toml
name = "hello"
version = "0.1.0"
description = "what it is for"
command = ["python", "-u", "server.py"]   # optional: server.py/.js/.mjs/.ps1/.exe are auto-detected
# enabled = false                         # or create a `.disabled` file / use the `plugin` tool
# prefix = true                           # tool `greet` is exposed as `hello_greet`
# timeout_ms = 0                          # per call, 0 = none
[env]
SOME_SETTING = "value"
```

## Python in 10 lines

```python
from nerve_plugin import Plugin, Image, ToolError

p = Plugin("hello")

@p.tool
def greet(name: str, times: int = 1) -> str:
    """Say hello.

    Args:
        name: who to greet
        times: how many times
    """
    return " ".join([f"hello {name}"] * times)

p.run()
```

* Schema comes from the signature + `Args:` docstring (or pass `schema=`).
* A `ctx` parameter gives `ctx.progress(msg)`, `ctx.check()` (cancellation), `ctx.log(...)` and
  `ctx.nerve("ui", action="screenshot")` to call nerve's built-in tools.
* Return text, dict/list (JSON), `Image(path)` (nerve fits it to the client's budget), or a list.
* `print()` goes to `<nerve data>/logs/plugins/<name>.log`.
* `spawn_detached([...])` starts apps that must outlive plugin reloads.
* The process runs from the plugins folder (so its own folder stays deletable); use
  `os.path.dirname(__file__)` or `NERVE_PLUGIN_DIR` for your files. Also set: `NERVE_OUT`, `NERVE_DATA`, `NERVE_URL`.

Any language works: answer `initialize`, `tools/list`, `tools/call` on stdin/stdout.

## Managing

The built-in `plugin` tool: `list`, `reload {name}`, `enable/disable {name}`, `logs {name}`,
`new {name}` (scaffold). Bundled plugins are updated with nerve unless you edit them:

* `blender`, `unreal` - drive the running editors.
* `montage` - video editing: probe/look/scenes/silences/transcribe/beats, real SFX and music
  (Openverse: Freesound + Jamendo, license-filtered; optional `FREESOUND_TOKEN`, own packs via
  `NERVE_MEDIA_DIRS`), JSON timeline -> mixed, captioned render (ffmpeg, NVENC), OTIO export.
* `skills` - a shared library of Agent Skills (SKILL.md): read before a task, record lessons after.
