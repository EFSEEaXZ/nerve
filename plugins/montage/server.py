"""nerve plugin: a video editing assistant. Real SFX/music libraries, analysis (eyes + ears), a timeline renderer
with pro audio defaults (per-role loudness, ducking, peak-aligned hits, loudness-targeted master), captions, OTIO export."""

import json
import os
import subprocess
import sys
import threading
import time
import uuid

from nerve_plugin import Image, Plugin, ToolError

import analysis as A
import engine as E
import sources as S

p = Plugin("montage", version="1.0.0")
_jobs = {}
_jlock = threading.Lock()
WAIT = float(os.environ.get("NERVE_MONTAGE_WAIT", "40"))


# ------------------------------------------------------------------ background jobs (renders outlive the client timeout)

def _async(kind, fn, a, ctx):
    jid = f"{kind}-{uuid.uuid4().hex[:6]}"
    job = {"id": jid, "kind": kind, "state": "running", "progress": "", "started": time.time(), "result": None, "error": None, "done": threading.Event()}

    def prog(msg, cur=None, total=None):
        job["progress"] = msg
        try:
            ctx.progress(msg, cur, total)
        except Exception:
            pass

    def worker():
        try:
            job["result"] = fn(prog)
            job["state"] = "done"
        except ToolError as e:
            job["error"], job["state"] = str(e), "failed"
        except Exception as e:
            job["error"], job["state"] = f"{type(e).__name__}: {e}", "failed"
        finally:
            job["finished"] = time.time()
            job["done"].set()

    with _jlock:
        _jobs[jid] = job
    threading.Thread(target=worker, daemon=True).start()
    return _wait(job, float(a.get("wait", WAIT)))


def _wait(job, wait):
    job["done"].wait(max(0.0, min(wait, 55.0)))
    if job["state"] == "running":
        return f"still running: job `{job['id']}` ({time.time() - job['started']:.0f}s, {job['progress'] or 'working'}). Poll with montage {{action:'job', id:'{job['id']}'}}."
    if job["state"] == "failed":
        raise ToolError(f"{job['kind']} failed: {job['error']}")
    return job["result"]


def act_job(a, ctx):
    jid = a.get("id")
    if not jid:
        rows = [f"{j['id']}: {j['state']} {j['progress']}" for j in _jobs.values()]
        return "\n".join(rows) or "no jobs"
    job = _jobs.get(jid)
    if not job:
        raise ToolError(f"no job {jid} (plugin restarted?)")
    return _post(job["kind"], _wait(job, float(a.get("wait", WAIT))), a)


# ------------------------------------------------------------------ formatting

def J(x):
    return json.dumps(x, ensure_ascii=False, separators=(",", ":"), default=str)


def _save_json(name, obj):
    d = os.path.join(A.OUT, "analysis")
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, name)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=0)
    return path


def _post(kind, res, a):
    """Shape a finished job result for the client."""
    if not isinstance(res, dict):
        return res
    if kind == "render":
        out = [J(res)]
        if a.get("check", True) is not False and os.path.exists(res["output"]):
            times = []
            for c in res["clips"]:
                times.append(min(res["duration"] - 0.05, (c["start"] + c["end"]) / 2))
            if len(times) > 16:
                step = len(times) / 16
                times = [times[int(i * step)] for i in range(16)]
            try:
                sheet, _ = A.contact_sheet(res["output"], times=times, width=1400)
                out.append("Frames of the render (one per clip, mid-point) - check framing, text, captions:")
                out.append(Image(path=sheet))
            except Exception as e:
                out.append(f"(no preview sheet: {e})")
        out.append("Next: fix anything off, re-render with preview:true for speed, then final. Afterwards: skills learn {name:<relevant installed skill>, lesson} with what you'd do differently.")
        return out
    if kind == "transcribe":
        segs = res["segments"]
        text = "\n".join(f"[{A.fmt_t(s['start'])}-{A.fmt_t(s['end'])}] {s['text']}" for s in segs[:400])
        more = f"\n... {len(segs) - 400} more segments in the json" if len(segs) > 400 else ""
        return f"{res['language']} · {len(res['words'])} words · {res['duration']}s · whisper {res['model']} on {res['device']}\nfiles: {J(res['files'])}\n(use files.json as captions.src)\n\n{text}{more}"
    if kind == "beats":
        path = _save_json(os.path.splitext(os.path.basename(res["path"]))[0] + ".beats.json", res)
        short = dict(res)
        if len(short["beats"]) > 96:
            short["beats"] = short["beats"][:96] + [f"... {len(res['beats']) - 96} more in file"]
        if len(short["energy_per_sec"]) > 240:
            short["energy_per_sec"] = short["energy_per_sec"][:240] + ["..."]
        short["file"] = path
        return J(short)
    return J(res)


# ------------------------------------------------------------------ actions

def act_status(a, ctx):
    import importlib.util as u
    deps = {m: bool(u.find_spec(m)) for m in ("numpy", "PIL", "librosa", "faster_whisper", "opentimelineio")}
    try:
        ff = A.run([A.which("ffmpeg"), "-version"]).stdout.splitlines()[0]
    except ToolError as e:
        ff = str(e)
    idx = S.load_index()
    kinds = {}
    for it in idx["items"].values():
        kinds[it.get("kind", "?")] = kinds.get(it.get("kind", "?"), 0) + 1
    return J({"ffmpeg": ff, "nvenc": E.nvenc_ok(), "python_deps": deps, "media_dir": S.MEDIA, "media_dirs": S.media_dirs(), "out_dir": A.OUT,
              "library": kinds, "sources": {"openverse": "on (Freesound + Jamendo, no key)", "freesound_api": "on" if S.freesound_token() else "off (set FREESOUND_TOKEN for better ranking + more results)"},
              "missing": [m for m, ok in deps.items() if not ok] or None})


def act_setup(a, ctx):
    pkgs = ["numpy", "pillow", "librosa", "soundfile", "faster-whisper", "opentimelineio"]
    r = subprocess.run([sys.executable, "-m", "pip", "install", "-q"] + pkgs, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=900)
    return (r.stdout + r.stderr)[-2000:] or "ok"


def act_probe(a, ctx):
    info = A.probe(a.get("path"))
    if a.get("loudness") and info.get("audio"):
        lu = A.loudness(info["path"])
        info["loudness"] = {k: v for k, v in (lu or {}).items() if k != "_raw"}
    return J(info)


def act_look(a, ctx):
    path = A.need(a.get("path"))
    times = a.get("times")
    sheet, ts = A.contact_sheet(path, a.get("count") or 12, a.get("start"), a.get("end"), a.get("cols"), int(a.get("width") or 1600),
                                times=[float(t) for t in times] if times else None)
    info = A.probe(path)
    return [f"{os.path.basename(path)} · {info['duration']}s · {(info.get('video') or {}).get('width')}x{(info.get('video') or {}).get('height')} · frames at {', '.join(A.fmt_t(t) for t in ts)}", Image(path=sheet)]


def act_frame(a, ctx):
    return Image(path=A.frame(a.get("path"), a.get("t") or a.get("time") or 0, width=int(a.get("width") or 1280)))


def act_scenes(a, ctx):
    return _async("scenes", lambda prog: A.scenes(a.get("path"), float(a.get("threshold") or 0.3)), a, ctx)


def act_silences(a, ctx):
    def run(prog):
        r = A.silences(a.get("path"), float(a.get("noise_db") or -35), float(a.get("min_silence") or 0.35), float(a.get("pad") or 0.08))
        src = A.need(a.get("path"))
        r["video_clips"] = [{"src": src, "in": k[0], "out": k[1]} for k in r["keep"]]
        r["hint"] = "paste video_clips into timeline.video for jump cuts (captions map automatically)"
        return r
    return _async("silences", run, a, ctx)


def act_transcribe(a, ctx):
    return _async("transcribe", lambda prog: A.transcribe(a.get("path"), a.get("language"), a.get("model"), a.get("device"), prog), a, ctx)


def act_beats(a, ctx):
    return _async("beats", lambda prog: A.beats(a.get("path"), a.get("start"), a.get("duration")), a, ctx)


def act_inspect(a, ctx):
    path = a.get("path")
    if path and str(path).startswith(("ov:", "fs:", "local:")):
        path = S.get(path)["path"]
    r = A.inspect_audio(A.need(path))
    rec = S.record_for_path(r["path"])
    if rec:
        S.update_record(rec["id"], duration=r["duration"], peak_time=r["peak_time"], shape=r["shape"])
        r["license"] = rec.get("license")
    wave = r.pop("waveform", None)
    return [J(r), Image(path=wave)] if wave else J(r)


def _search(a, kind):
    q = a.get("query") or a.get("q")
    res, errors = S.search(q, kind, a.get("source") or "all", a.get("min_duration"), a.get("max_duration") or (8 if kind == "sfx" and not a.get("min_duration") else None),
                           int(a.get("limit") or 8), a.get("license") or "safe")
    lines = [f"{len(res)} {kind} result(s) for `{q}`" + (f" (errors: {'; '.join(errors)})" if errors else "")]
    for r in res:
        bits = [r["id"], f"{r.get('duration')}s" if r.get("duration") else "?s", r.get("license") or "", r.get("source") or ""]
        if r.get("rating"):
            bits.append(f"★{r['rating']}")
        if r.get("downloads"):
            bits.append(f"{r['downloads']} dl")
        tags = ", ".join((r.get("tags") or [])[:6])
        lines.append(f"- {r.get('title')!s:.60} | " + " · ".join(b for b in bits if b) + (f" | {tags}" if tags else "") + (f" | by {r['author']}" if r.get("author") else ""))
    n = int(a.get("get") or 0)
    if n and res:
        lines.append("")
        for r in res[:n]:
            try:
                rec = S.get(r["id"], kind)
                ins = A.inspect_audio(rec["path"], image=False)
                S.update_record(rec["id"], duration=ins["duration"], peak_time=ins["peak_time"], shape=ins["shape"])
                lines.append(f"downloaded {r['id']} -> {rec['path']} | {ins['duration']}s, peak at {ins['peak_time']}s, {ins['shape']}, {ins['tone']}")
            except Exception as e:
                lines.append(f"download {r['id']} failed: {e}")
    elif res:
        lines.append(f"Next: montage {{action:'get', id:'{res[0]['id']}'}} (or pass get:N to fetch the top N). Try 2-3 candidates and `inspect` them; vary sounds across the edit.")
    else:
        lines.append("Nothing found: try simpler English words (whoosh, swoosh, impact, hit, riser, boom, click, pop, glitch, cinematic, ambience, crowd) or another source.")
    return "\n".join(lines)


def act_sfx(a, ctx):
    return _search(a, "sfx")


def act_music(a, ctx):
    if not a.get("min_duration") and not a.get("max_duration"):
        a = dict(a, min_duration=30)
    return _search(a, "music")


def act_get(a, ctx):
    ids = a.get("ids") or ([a["id"]] if a.get("id") else [])
    if not ids:
        raise ToolError("`id` (or `ids`) from sfx/music results is required")
    out = []
    for i in ids:
        rec = S.get(i, a.get("kind"))
        ins = A.inspect_audio(rec["path"], image=False)
        S.update_record(rec["id"], duration=ins["duration"], peak_time=ins["peak_time"], shape=ins["shape"])
        out.append({"id": rec["id"], "path": rec["path"], "license": rec.get("license"), "credit": rec.get("attribution"),
                    "duration": ins["duration"], "peak_time": ins["peak_time"], "shape": ins["shape"], "tone": ins["tone"], "lufs": ins.get("lufs")})
    return J(out if len(out) > 1 else out[0])


def act_library(a, ctx):
    if a.get("scan") or a.get("op") == "scan":
        n, total = S.scan(lambda m: ctx.progress(m))
        return f"indexed {n} new file(s); library has {total}. Folders: {S.media_dirs()} (add yours via NERVE_MEDIA_DIRS in plugins/montage/plugin.toml)"
    q = a.get("query")
    items = S.search_local(q, a.get("kind"), limit=int(a.get("limit") or 30)) if q else list(S.load_index()["items"].values())[: int(a.get("limit") or 30)]
    return "\n".join(f"- {it['id']} | {it.get('kind')} | {it.get('title')} | {it.get('duration', '?')}s | {it.get('license')} | {it['path']}" for it in items) or "library is empty - use sfx/music + get, or add your packs via NERVE_MEDIA_DIRS and library {scan:true}"


def _timeline(a):
    tl = a.get("timeline") or a.get("file")
    if tl is None:
        raise ToolError("`timeline` is required (object, JSON text, or path to a .json). See the tool description for the format.")
    return tl


def act_plan(a, ctx):
    return J(E.summary(E.plan(_timeline(a))))


def act_render(a, ctx):
    tl = _timeline(a)
    E.plan(tl)  # validate synchronously -> fast, clear errors
    return _post("render", _async("render", lambda prog: E.render(tl, preview=bool(a.get("preview")), encoder=a.get("encoder") or "auto",
                                                                  progress=prog, output=a.get("output")), a, ctx), a)


def act_export(a, ctx):
    return J(E.export_otio(_timeline(a), output=a.get("output")))


def act_credits(a, ctx):
    if a.get("timeline") or a.get("file"):
        P = E.plan(_timeline(a))
        path = E.write_credits(P, a.get("output") or os.path.join(A.OUT, "CREDITS.txt"))
        return open(path, encoding="utf-8").read() if path else "no downloaded assets need credit in this timeline"
    items = [it for it in S.load_index()["items"].values() if it.get("source") != "local"]
    return "\n".join(it.get("attribution") or it.get("title", "") for it in items) or "nothing downloaded yet"


ACTIONS = {
    "status": act_status, "setup": act_setup, "probe": act_probe, "look": act_look, "frame": act_frame, "scenes": act_scenes,
    "silences": act_silences, "transcribe": act_transcribe, "beats": act_beats, "inspect": act_inspect, "sfx": act_sfx,
    "music": act_music, "get": act_get, "library": act_library, "plan": act_plan, "render": act_render, "export": act_export,
    "credits": act_credits, "job": act_job,
}
for _k in ("scenes", "silences", "transcribe", "beats"):
    _f = ACTIONS[_k]
    ACTIONS[_k] = (lambda f, k: (lambda a, ctx: _post(k, f(a, ctx), a)))(_f, _k)

SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": sorted(ACTIONS)},
        "path": {"type": "string", "description": "media file (probe/look/frame/scenes/silences/transcribe/beats/inspect)"},
        "query": {"type": "string", "description": "sfx/music/library search, English works best"},
        "id": {"type": "string", "description": "get/inspect: id from search results (ov:/fs:/local:) / job: job id"},
        "ids": {"type": "array", "items": {"type": "string"}},
        "get": {"type": "integer", "description": "sfx/music: download + analyse the top N right away"},
        "source": {"type": "string", "description": "sfx/music: all | local | freesound | openverse (comma list ok)"},
        "license": {"type": "string", "enum": ["safe", "commercial", "any"], "description": "safe (default) = CC0/BY/BY-SA: commercial use + editing allowed"},
        "min_duration": {"type": "number"}, "max_duration": {"type": "number"},
        "limit": {"type": "integer"},
        "timeline": {"description": "plan/render/export/credits: timeline object, JSON text or .json path"},
        "preview": {"type": "boolean", "description": "render: fast low-res draft"},
        "output": {"type": "string"},
        "encoder": {"type": "string", "enum": ["auto", "nvenc", "x264"]},
        "check": {"type": "boolean", "description": "render: return a frame sheet of the result (default true)"},
        "wait": {"type": "number", "description": "seconds to wait before returning a job id (default 40)"},
        "t": {"type": "number", "description": "frame: time in seconds"},
        "times": {"type": "array", "items": {"type": "number"}, "description": "look: exact times"},
        "count": {"type": "integer"}, "start": {"type": "number"}, "end": {"type": "number"}, "duration": {"type": "number"},
        "cols": {"type": "integer"}, "width": {"type": "integer"},
        "threshold": {"type": "number", "description": "scenes: 0.2 sensitive .. 0.5 strict"},
        "noise_db": {"type": "number"}, "min_silence": {"type": "number"}, "pad": {"type": "number"},
        "language": {"type": "string"}, "model": {"type": "string", "description": "whisper: tiny|base|small|medium|large-v3|turbo"},
        "device": {"type": "string"}, "loudness": {"type": "boolean"}, "scan": {"type": "boolean"}, "kind": {"type": "string"},
    },
    "required": ["action"],
}

DESC = (
    "Video editing assistant (montage). First use skills find to look for relevant installed instructions; read them if available. "
    "NEVER synthesize sound effects or music with code (sine/noise beeps sound cheap) - always use real recordings: "
    "sfx {query, get?} / music {query} search Freesound + Jamendo (via Openverse, license-safe by default) and the local library; get {id} downloads and analyses (peak time, shape, tone). "
    "Eyes/ears: probe {path}; look {path, count?} (frame grid with timestamps); frame {path, t}; scenes {path}; silences {path} (speech ranges -> jump-cut clips); "
    "transcribe {path, language?} (word timestamps, captions); beats {path} (tempo, beats, downbeats, hits/drops, sections); inspect {path|id} (waveform + where the hit is). "
    "Edit: plan {timeline} (resolved cut times, warnings) -> render {timeline, preview?} -> export {timeline} (.otio for Resolve/Premiere/Kdenlive); credits. Long work returns a job id -> job {id}. "
    "Timeline: {size:'1080p'|'vertical'|[w,h], fps, transition?, grade?:'cinematic'|'punchy'|'warm'|{contrast,saturation,...}, "
    "video:[{src, in, out | duration, speed?, transition?:{type: fade|fadeblack|dissolve|slideleft|smoothleft|zoomin|circleopen|wipeleft|hblur..., duration}, zoom?:{from,to,x,y}, punch?:1.15, fit?:cover|contain|blur, audio?:false, volume_db?} | {image, duration} | {color, duration}], "
    "overlays:[{text, at, duration, pos:top|center|bottom|lower-third, size, color, box?, font?} | {src|image, at, duration, w?, x?, y?, opacity?}], "
    "audio:[{src|id, role: music|sfx|ambience|voice, at, in?, duration?|until?, align?:'peak'|'end', volume_db?, fade_in?, fade_out?, loop?}], "
    "captions?:{src: <transcript.json>, style: pop|clean, highlight?}, mix?:{loudness:'youtube'|-14, duck?:true}}. "
    "Times accept numbers or 'cut:N' (the perceived cut into clip N), 'clip:N', 'clipend:N', 'end', with +/- offsets, e.g. at:'cut:3', align:'peak' lands an impact exactly on the cut; a whoosh at 'cut:3-0.25'; align:'end' makes the sound END on at (risers into a reveal). "
    "Levels are automatic: every item is loudness-matched to its role (music becomes a ducked bed under speech), volume_db is a relative tweak; master is 2-pass normalised. "
    "Loop: look at the footage -> plan -> preview render -> check the frame sheet -> fix -> final render -> skills learn."
)


@p.tool(schema=SCHEMA, description=DESC, annotations={"title": "Montage", "destructiveHint": False, "openWorldHint": True})
def montage(ctx, **a):
    act = a.get("action") or "status"
    fn = ACTIONS.get(act)
    if fn is None:
        raise ToolError(f"unknown action `{act}`; valid: {', '.join(sorted(ACTIONS))}")
    return fn(a, ctx)


if __name__ == "__main__":
    p.run()
