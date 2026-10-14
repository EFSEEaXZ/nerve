"""Timeline JSON -> plan -> ffmpeg (audio mix, video, mux with 2-pass loudness). Also OTIO export."""

import glob
import json
import os
import re
import shutil
import subprocess
import time

from nerve_plugin import ToolError

import analysis as A
import sources as S

PRESETS = {"1080p": (1920, 1080), "720p": (1280, 720), "4k": (3840, 2160), "vertical": (1080, 1920), "shorts": (1080, 1920),
           "tiktok": (1080, 1920), "reels": (1080, 1920), "square": (1080, 1080), "portrait": (1080, 1350)}
XFADE = {"fade", "fadeblack", "fadewhite", "fadegrays", "dissolve", "distance", "pixelize", "radial", "hblur", "zoomin",
         "wipeleft", "wiperight", "wipeup", "wipedown", "wipetl", "wipetr", "wipebl", "wipebr",
         "slideleft", "slideright", "slideup", "slidedown", "smoothleft", "smoothright", "smoothup", "smoothdown",
         "circleopen", "circleclose", "circlecrop", "rectcrop", "vertopen", "vertclose", "horzopen", "horzclose",
         "diagtl", "diagtr", "diagbl", "diagbr", "hlslice", "hrslice", "vuslice", "vdslice", "squeezeh", "squeezev",
         "coverleft", "coverright", "coverup", "coverdown", "revealleft", "revealright", "revealup", "revealdown",
         "hlwind", "hrwind", "vuwind", "vdwind"}
# integrated-loudness targets per role (before the master loudnorm). "bed" = music under speech.
ROLE_LUFS = {"voice": -16.0, "music": -16.0, "bed": -27.0, "sfx": -19.0, "ambience": -32.0}
PLATFORM_LUFS = {"youtube": -14, "tiktok": -14, "instagram": -14, "reels": -14, "shorts": -14, "spotify": -14,
                 "podcast": -16, "apple": -16, "broadcast": -23, "ebu": -23, "film": -24}
_nvenc = None
_lufs_cache = {}


def err(where, msg):
    raise ToolError(f"timeline {where}: {msg}")


def _num(v, where, default=None):
    if v is None:
        return default
    try:
        return float(v)
    except (TypeError, ValueError):
        err(where, f"expected a number, got {v!r}")


def _path(v, base, where):
    if not v:
        err(where, "missing src")
    v = str(v)
    if v.startswith(("ov:", "fs:", "local:")):
        return S.get(v)["path"]
    p = os.path.expandvars(os.path.expanduser(v))
    if not os.path.isabs(p):
        for b in (base, A.OUT, S.MEDIA):
            if b and os.path.exists(os.path.join(b, p)):
                p = os.path.join(b, p)
                break
    if not os.path.isfile(p):
        err(where, f"file not found: {v}")
    return os.path.abspath(p)


def _font(name):
    if name and os.path.isfile(str(name)):
        return str(name)
    dirs = [os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "Fonts"),
            os.path.join(os.environ.get("LOCALAPPDATA", ""), "Microsoft", "Windows", "Fonts"),
            "/usr/share/fonts", "/Library/Fonts", os.path.expanduser("~/Library/Fonts")]
    want = re.sub(r"[^a-z0-9]", "", str(name or "arialbd").lower())
    files = [f for d in dirs if os.path.isdir(d) for f in glob.glob(os.path.join(d, "**", "*.[ot]tf"), recursive=True)]
    for f in files:
        if re.sub(r"[^a-z0-9]", "", os.path.splitext(os.path.basename(f))[0].lower()) == want:
            return f
    for f in files:
        if want and want in re.sub(r"[^a-z0-9]", "", os.path.basename(f).lower()):
            return f
    for f in files:
        if os.path.basename(f).lower() in ("arialbd.ttf", "dejavusans-bold.ttf"):
            return f
    return files[0] if files else None


def parse_time(v, plan, where):
    """number | 'cut:N' (perceived cut into clip N) | 'clip:N' (clip start) | 'clipend:N' | 'end', each optionally +/-x."""
    if v is None:
        return 0.0
    if isinstance(v, (int, float)):
        return float(v)
    m = re.match(r"^\s*(cut|clip|clipend|end)(?::(\d+))?\s*(?:([+-])\s*([\d.]+))?\s*$", str(v))
    if not m:
        return _num(v, where)
    kind, idx, sign, off = m.groups()
    clips = plan["clips"]
    if kind == "end":
        t = plan["duration"]
    else:
        if idx is None or int(idx) >= len(clips):
            err(where, f"{kind}:{idx} - there are {len(clips)} clips (0-based)")
        c = clips[int(idx)]
        t = c["cut"] if kind == "cut" else c["start"] if kind == "clip" else c["end"]
    if off:
        t += float(off) * (1 if sign == "+" else -1)
    return t


# ------------------------------------------------------------------ plan

def plan(tl, base=None):
    if isinstance(tl, str):
        p = tl
        if os.path.isfile(p):
            base = base or os.path.dirname(os.path.abspath(p))
            with open(p, encoding="utf-8") as f:
                tl = json.load(f)
        else:
            try:
                tl = json.loads(tl)
            except Exception:
                raise ToolError("timeline must be an object, JSON text or a .json file path")
    if not isinstance(tl, dict):
        raise ToolError("timeline must be an object")
    size = tl.get("size") or tl.get("preset") or "1080p"
    if isinstance(size, str):
        if size.lower() in PRESETS:
            W, H = PRESETS[size.lower()]
        elif re.match(r"^\d+x\d+$", size):
            W, H = map(int, size.split("x"))
        else:
            err("size", f"unknown preset {size}; use {', '.join(PRESETS)} or [w,h]")
    else:
        W, H = int(size[0]), int(size[1])
    W, H = W // 2 * 2, H // 2 * 2
    fps = float(tl.get("fps") or 30)
    bg = tl.get("background") or "black"
    warnings = []
    default_tr = tl.get("transition")
    clips, t = [], 0.0
    for i, c in enumerate(tl.get("video") or []):
        w = f"video[{i}]"
        if isinstance(c, str):
            c = {"src": c}
        c = dict(c)
        speed = _num(c.get("speed"), w + ".speed", 1.0)
        if speed <= 0:
            err(w, "speed must be > 0")
        kind, src, info = "video", None, None
        if c.get("color") and not (c.get("src") or c.get("image")):
            kind = "color"
            dur = _num(c.get("duration"), w, 2.0)
            cin, cout = 0.0, dur
        else:
            src = _path(c.get("src") or c.get("image"), base, w + ".src")
            info = A.probe(src)
            if info.get("image") or not info.get("video"):
                if not info.get("video") and not info.get("image"):
                    err(w, f"{os.path.basename(src)} has no video stream (put audio files in `audio`)")
                kind = "image"
                dur = _num(c.get("duration"), w, 3.0)
                cin, cout = 0.0, dur
                speed = 1.0
            else:
                cin = _num(c.get("in"), w + ".in", 0.0)
                cout = c.get("out")
                if cout is None and c.get("duration") is not None:
                    cout = cin + _num(c.get("duration"), w) * speed
                cout = _num(cout, w + ".out", info["duration"])
                if cout > info["duration"] + 0.01:
                    warnings.append(f"{w}: out {cout} > source duration {info['duration']}, clamped")
                    cout = info["duration"]
                if cout - cin < 0.05:
                    err(w, f"empty range in={cin} out={cout}")
        d = (cout - cin) / speed
        tr = c.get("transition", default_tr if i else None)
        if isinstance(tr, str):
            tr = {"type": tr}
        trt, trd = None, 0.0
        if tr and i > 0 and tr.get("type") not in (None, "cut", "none"):
            trt = tr["type"]
            if trt not in XFADE:
                err(w + ".transition", f"unknown `{trt}`; e.g. fade, fadeblack, dissolve, slideleft, smoothleft, circleopen, zoomin, wipeleft, hblur")
            trd = _num(tr.get("duration"), w + ".transition.duration", 0.4)
            lim = min(d, clips[-1]["dur"]) * 0.9
            if trd > lim:
                warnings.append(f"{w}: transition {trd}s too long for the clips, shortened to {lim:.2f}s")
                trd = lim
        start = t - trd if i else 0.0
        clips.append({
            "i": i, "kind": kind, "src": src, "in": round(cin, 4), "out": round(cout, 4), "speed": speed, "dur": d,
            "start": start, "end": start + d, "cut": start + trd / 2, "transition": trt, "tdur": trd,
            "has_audio": bool(info and info.get("audio")) and kind == "video" and c.get("audio", True) is not False,
            "volume_db": _num(c.get("volume_db", c.get("volume")), w + ".volume_db", 0.0),
            "fit": c.get("fit") or tl.get("fit"), "zoom": c.get("zoom"), "punch": c.get("punch"), "color": c.get("color") if kind != "color" else None,
            "fill": c.get("color") if kind == "color" else None, "hflip": bool(c.get("hflip")), "lut": c.get("lut"),
            "src_size": (info["video"]["width"], info["video"]["height"]) if info and info.get("video") else None,
            "label": c.get("label") or (os.path.basename(src) if src else kind),
        })
        t = start + d
    if not clips:
        raise ToolError("timeline.video is empty: add clips like {\"src\": \"a.mp4\", \"in\": 1.5, \"out\": 4}")
    P = {"W": W, "H": H, "fps": fps, "bg": bg, "clips": clips, "duration": t, "warnings": warnings, "base": base,
         "grade": tl.get("grade"), "output": tl.get("output"), "raw": tl}
    if tl.get("duration"):
        P["duration"] = min(t, _num(tl["duration"], "duration"))
    # overlays
    ovs = []
    for i, o in enumerate(tl.get("overlays") or []):
        w = f"overlays[{i}]"
        o = dict(o)
        at = parse_time(o.get("at"), P, w + ".at")
        if o.get("text") is not None:
            dur = _num(o.get("duration"), w, 2.5)
            ovs.append(dict(o, type="text", at=at, dur=dur))
            continue
        src = _path(o.get("src") or o.get("image"), base, w + ".src")
        info = A.probe(src)
        isimg = info.get("image") or not info.get("audio") and info["duration"] < 0.1
        oin = _num(o.get("in"), w, 0.0)
        dur = _num(o.get("duration"), w, None)
        if dur is None:
            dur = (_num(o.get("out"), w, info["duration"]) - oin) if not isimg else 3.0
        if not isimg and oin + dur > info["duration"] + 0.01:
            dur = max(0.05, info["duration"] - oin)
            warnings.append(f"{w}: clipped to source length")
        ovs.append(dict(o, type="image" if isimg else "video", src=src, at=at, dur=dur, **{"in": oin},
                        has_audio=bool(info.get("audio")) and o.get("audio") is True))
    P["overlays"] = ovs
    # audio
    has_voice = any(c["has_audio"] for c in clips) and (tl.get("mix") or {}).get("clip_role", "voice") == "voice"
    items = []
    for i, a in enumerate(tl.get("audio") or []):
        w = f"audio[{i}]"
        if isinstance(a, str):
            a = {"src": a}
        a = dict(a)
        role = (a.get("role") or ("music" if re.search(r"(?i)music|song|track|bgm", str(a.get("src", ""))) else "sfx")).lower()
        if role not in ROLE_LUFS:
            err(w + ".role", "use voice | music | sfx | ambience")
        if role == "voice":
            has_voice = True
        src = _path(a.get("src") or a.get("id"), base, w + ".src")
        info = A.probe(src)
        if not info.get("audio"):
            err(w, f"{os.path.basename(src)} has no audio")
        ain = _num(a.get("in"), w + ".in", 0.0)
        at = parse_time(a.get("at"), P, w + ".at")
        if a.get("offset") is not None:
            at += _num(a["offset"], w + ".offset")
        loop = bool(a.get("loop"))
        if a.get("out") is not None:
            dur = _num(a["out"], w) - ain
        elif a.get("duration") is not None:
            dur = _num(a["duration"], w)
        elif a.get("until") is not None:
            dur = parse_time(a["until"], P, w + ".until") - at
        elif loop or role in ("music", "ambience"):
            dur = P["duration"] - at
        else:
            dur = info["duration"] - ain
        if not loop:
            dur = min(dur, info["duration"] - ain)
        align = a.get("align") or "start"
        peak = None
        if align == "end":  # the sound ENDS on `at` (risers, reverse swells into a cut)
            at -= dur
            if at < 0:
                ain += -at
                dur += at
                at = 0.0
        if align == "peak":
            pk = (S.record_for_path(src) or {}).get("peak_time")
            if pk is None:
                pk = A.inspect_audio(src, image=False)["peak_time"]
            peak = pk
            shift = pk - ain
            at -= shift
            if at < 0:
                ain += -at
                dur += at
                at = 0.0
        if at >= P["duration"] or dur <= 0.01:
            warnings.append(f"{w}: starts after the end of the video or is empty - skipped")
            continue
        dur = min(dur, P["duration"] - at)
        items.append({"i": i, "src": src, "role": role, "at": round(at, 4), "in": round(ain, 4), "dur": round(dur, 4), "loop": loop,
                      "gain": _num(a.get("volume_db", a.get("volume", a.get("gain_db"))), w + ".volume_db", 0.0),
                      "normalize": a.get("normalize", True) is not False,
                      "fade_in": _num(a.get("fade_in"), w, 0.0 if role == "sfx" else 0.3 if role == "music" else 0.0),
                      "fade_out": _num(a.get("fade_out"), w, 0.0 if role == "sfx" else 1.5 if role == "music" else 0.0),
                      "pan": _num(a.get("pan"), w + ".pan", 0.0), "peak": peak, "label": a.get("label") or os.path.basename(src)})
    P["audio"] = items
    P["has_voice"] = has_voice
    mix = dict(tl.get("mix") or {})
    target = mix.get("loudness", mix.get("platform", "youtube"))
    P["lufs"] = float(PLATFORM_LUFS.get(str(target).lower(), target)) if not isinstance(target, (int, float)) else float(target)
    P["true_peak"] = float(mix.get("true_peak", -1.0))
    P["duck"] = mix.get("duck", True) is not False
    P["duck_db"] = float(mix.get("duck_db", 12))
    P["clip_role"] = mix.get("clip_role", "voice")
    P["captions"] = tl.get("captions")
    if P["captions"] and isinstance(P["captions"], str):
        P["captions"] = {"src": P["captions"]}
    _lint(P)
    return P


def _lint(P):
    w = P["warnings"]
    sfx = sorted((a for a in P["audio"] if a["role"] == "sfx"), key=lambda a: a["at"])
    for a, b in zip(sfx, sfx[1:]):
        if abs(a["at"] - b["at"]) < 0.05 and a["src"] == b["src"]:
            w.append(f"audio[{a['i']}] and audio[{b['i']}]: same sound twice at the same time (phasing)")
    srcs = [a["src"] for a in sfx]
    for s in set(srcs):
        if srcs.count(s) >= 4:
            w.append(f"{os.path.basename(s)} is used {srcs.count(s)}x - vary sounds (different whooshes/impacts) so the edit doesn't feel cheap")
    for a in P["audio"]:
        rec = S.record_for_path(a["src"]) or {}
        lic = rec.get("license", "")
        if "nc" in lic.split("-") or lic.startswith("nc"):
            w.append(f"{os.path.basename(a['src'])}: license {lic} is NON-commercial")
        if lic in ("by-nd", "by-nc-nd"):
            w.append(f"{os.path.basename(a['src'])}: license {lic} forbids edits/sync to video")
    long_clips = [c for c in P["clips"] if c["kind"] != "color" and c["dur"] > 12 and not c["has_audio"]]
    if long_clips:
        w.append(f"{len(long_clips)} silent clip(s) longer than 12s - consider tighter cuts, zoom or b-roll")


def summary(P):
    rows = []
    for c in P["clips"]:
        rows.append({"clip": c["i"], "label": c["label"], "start": round(c["start"], 3), "end": round(c["end"], 3),
                     "cut": round(c["cut"], 3), "src_range": [c["in"], c["out"]] if c["kind"] == "video" else None,
                     **({"transition": f"{c['transition']} {c['tdur']:.2f}s"} if c["transition"] else {})})
    aud = [{"audio": a["i"], "role": a["role"], "label": a["label"], "at": a["at"], "dur": a["dur"],
            **({"peak_at": round(a["at"] + (a["peak"] - a["in"]), 3)} if a.get("peak") is not None else {})} for a in P["audio"]]
    return {"size": [P["W"], P["H"]], "fps": P["fps"], "duration": round(P["duration"], 3), "clips": rows, "audio": aud,
            "overlays": len(P["overlays"]), "voice": P["has_voice"], "loudness_target": P["lufs"], "warnings": P["warnings"]}


# ------------------------------------------------------------------ helpers for graphs

def _atempo(speed):
    out, s = [], float(speed)
    while s > 2.0:
        out.append("atempo=2.0")
        s /= 2.0
    while s < 0.5:
        out.append("atempo=0.5")
        s /= 0.5
    if abs(s - 1) > 1e-6:
        out.append(f"atempo={s:.6f}")
    return out


def _fit_mode(c, W, H):
    f = c["fit"]
    if f:
        return f
    if not c["src_size"]:
        return "cover"
    sw, sh = c["src_size"]
    r1, r2 = sw / sh, W / H
    return "cover" if max(r1, r2) / min(r1, r2) < 1.25 else "blur"


def _color(spec):
    if not spec:
        return []
    if isinstance(spec, str):
        spec = {"cinematic": {"contrast": 1.08, "saturation": 0.92, "gamma": 0.97},
                "punchy": {"contrast": 1.12, "saturation": 1.18},
                "warm": {"contrast": 1.04, "saturation": 1.05, "warm": 0.06},
                "cold": {"contrast": 1.05, "saturation": 0.95, "warm": -0.06},
                "bw": {"saturation": 0.0, "contrast": 1.15}}.get(spec.lower(), {})
    f = []
    eq = {k: spec[k] for k in ("contrast", "saturation", "brightness", "gamma") if k in spec}
    if eq:
        f.append("eq=" + ":".join(f"{k}={float(v):.4f}" for k, v in eq.items()))
    if spec.get("warm"):
        k = float(spec["warm"])
        f.append(f"colorbalance=rm={k:.3f}:bm={-k:.3f}:rh={k / 2:.3f}:bh={-k / 2:.3f}")
    if spec.get("vignette"):
        f.append("vignette=PI/5")
    if spec.get("sharpen"):
        f.append("unsharp=5:5:0.6")
    return f


def _ffesc(s):
    return "'" + str(s).replace("\\", "/").replace(":", "\\:").replace("'", "\\'") + "'"


def _hex_ass(c, alpha=0):
    c = str(c).lstrip("#")
    names = {"white": "FFFFFF", "black": "000000", "yellow": "FFE600", "red": "FF3B30", "green": "34C759", "blue": "0A84FF", "cyan": "00E5FF", "pink": "FF2D95", "orange": "FF9500"}
    c = names.get(c.lower(), c)
    if len(c) != 6:
        c = "FFFFFF"
    return f"&H{alpha:02X}{c[4:6]}{c[2:4]}{c[0:2]}".upper()


# ------------------------------------------------------------------ captions (ASS)

def _map_words(P, tr):
    src = os.path.abspath(tr.get("source") or "").lower()
    words = tr.get("words") or []
    segs = [(c["in"], c["out"], c["speed"], c["start"] + (c["tdur"] if c["transition"] else 0) * 0, c["end"]) for c in P["clips"]
            if c["src"] and os.path.abspath(c["src"]).lower() == src and c["has_audio"]]
    segs += [(a["in"], a["in"] + a["dur"], 1.0, a["at"], a["at"] + a["dur"]) for a in P["audio"] if a["role"] == "voice" and os.path.abspath(a["src"]).lower() == src]
    if not segs:
        return [dict(w) for w in words], False
    out = []
    for w in words:
        for cin, cout, sp, t0, t1 in segs:
            if cin - 0.02 <= w["start"] < cout:
                s = t0 + (w["start"] - cin) / sp
                e = min(t1, t0 + (min(w["end"], cout) - cin) / sp)
                if e > s:
                    out.append({"word": w["word"], "start": s, "end": e})
                break
    return out, True


def _ass_time(t):
    cs = int(round(max(0, t) * 100))
    h, cs = divmod(cs, 360000)
    m, cs = divmod(cs, 6000)
    s, cs = divmod(cs, 100)
    return f"{h}:{m:02}:{s:02}.{cs:02}"


def build_ass(P, cap, path):
    src = cap.get("src") or cap.get("transcript")
    if not src:
        raise ToolError("captions.src: path to a .transcript.json from `transcribe` (or .srt)")
    src = _path(src, P["base"], "captions.src")
    if src.lower().endswith(".srt"):
        words = []
        blocks = re.split(r"\n\s*\n", open(src, encoding="utf-8-sig").read().strip())
        for b in blocks:
            m = re.search(r"(\d+):(\d+):(\d+)[,.](\d+)\s*-->\s*(\d+):(\d+):(\d+)[,.](\d+)\s*\n(.*)", b, re.S)
            if not m:
                continue
            g = list(map(int, m.groups()[:8]))
            s = g[0] * 3600 + g[1] * 60 + g[2] + g[3] / 1000
            e = g[4] * 3600 + g[5] * 60 + g[6] + g[7] / 1000
            ws = m.group(9).split()
            for k, wd in enumerate(ws):
                words.append({"word": wd, "start": s + (e - s) * k / len(ws), "end": s + (e - s) * (k + 1) / len(ws)})
        tr = {"words": words, "source": cap.get("source")}
    else:
        tr = json.load(open(src, encoding="utf-8"))
    words, mapped = _map_words(P, tr)
    off = float(cap.get("offset") or 0)
    for w in words:
        w["start"] += off
        w["end"] += off
    W, H = P["W"], P["H"]
    vertical = H > W
    style = cap.get("style") or ("pop" if vertical else "clean")
    size = int(cap.get("size") or (H * (0.052 if vertical else 0.06)) if style == "pop" else cap.get("size") or H * 0.048)
    font = cap.get("font") or ("Arial Black" if style == "pop" else "Arial")
    color, hl = cap.get("color") or "white", cap.get("highlight") or "#FFE600"
    pos = cap.get("position") or ("center" if vertical and style == "pop" else "bottom")
    align = {"bottom": 2, "center": 5, "top": 8}.get(pos, 2)
    mv = int(H * (0.30 if pos == "bottom" and vertical else 0.08 if pos == "bottom" else 0.05))
    upper = cap.get("uppercase", style == "pop")
    outline = int(cap.get("outline") or max(2, size // 12))
    head = ["[Script Info]", "ScriptType: v4.00+", f"PlayResX: {W}", f"PlayResY: {H}", "WrapStyle: 0", "ScaledBorderAndShadow: yes", "",
            "[V4+ Styles]",
            "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding",
            f"Style: Cap,{font},{size},{_hex_ass(color)},{_hex_ass(hl)},&H00000000,&H64000000,-1,0,0,0,100,100,0,0,1,{outline},{max(1, outline // 2)},{align},{int(W * 0.08)},{int(W * 0.08)},{mv},1",
            "", "[Events]", "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text"]
    ev = []
    clean = lambda s: s.replace("{", "(").replace("}", ")").replace("\\", "/")
    if style == "pop":
        n = int(cap.get("words_per_line") or 3)
        chunks, cur = [], []
        for w in words:
            if cur and (len(cur) >= n or w["start"] - cur[-1]["end"] > 0.6 or re.search(r"[.!?,;:]$", cur[-1]["word"])):
                chunks.append(cur)
                cur = []
            cur.append(w)
        if cur:
            chunks.append(cur)
        for ci, ch in enumerate(chunks):
            nxt = chunks[ci + 1][0]["start"] if ci + 1 < len(chunks) else ch[-1]["end"] + 0.4
            end_all = min(nxt, ch[-1]["end"] + 0.4)
            for k, w in enumerate(ch):
                s = w["start"] if k else ch[0]["start"]
                e = ch[k + 1]["start"] if k + 1 < len(ch) else end_all
                parts = []
                for j, x in enumerate(ch):
                    t = clean(x["word"].upper() if upper else x["word"])
                    parts.append("{\\1c" + _hex_ass(hl) + "\\fscx108\\fscy108}" + t + "{\\1c" + _hex_ass(color) + "\\fscx100\\fscy100}" if j == k else t)
                pre = "{\\fad(60,0)\\t(0,90,\\fscx100\\fscy100)\\fscx85\\fscy85}" if k == 0 else ""
                ev.append(f"Dialogue: 0,{_ass_time(s)},{_ass_time(e)},Cap,,0,0,0,,{pre}" + " ".join(parts))
    else:
        maxc = int(cap.get("max_chars") or (28 if vertical else 42))
        lines, cur = [], []
        for w in words:
            txt = " ".join(x["word"] for x in cur + [w])
            if cur and (len(txt) > maxc or w["start"] - cur[-1]["end"] > 0.8 or re.search(r"[.!?]$", cur[-1]["word"])):
                lines.append(cur)
                cur = []
            cur.append(w)
        if cur:
            lines.append(cur)
        for li, ln in enumerate(lines):
            nxt = lines[li + 1][0]["start"] if li + 1 < len(lines) else ln[-1]["end"] + 0.6
            txt = clean(" ".join(x["word"] for x in ln))
            ev.append(f"Dialogue: 0,{_ass_time(ln[0]['start'])},{_ass_time(min(nxt, ln[-1]['end'] + 0.6))},Cap,,0,0,0,,{txt.upper() if upper else txt}")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(head + ev) + "\n")
    return {"events": len(ev), "words": len(words), "mapped_to_timeline": mapped, "style": style, "font": font}


# ------------------------------------------------------------------ video graph

def video_graph(P, job, scale=1.0):
    W, H = int(P["W"] * scale) // 2 * 2, int(P["H"] * scale) // 2 * 2
    fps = P["fps"]
    bg = P["bg"]
    inputs, f = [], []
    fps_s = f"{fps:g}"

    def add_input(args):
        inputs.append(args)
        return len(inputs) - 1

    grade = _color(P.get("grade"))
    labels = []
    for c in P["clips"]:
        i, d = c["i"], c["dur"]
        if c["kind"] == "color":
            k = add_input(["-f", "lavfi", "-t", f"{d:.4f}", "-i", f"color=c={c['fill'] or 'black'}:s={W}x{H}:r={fps_s}"])
            chain = [f"[{k}:v]format=yuv420p"]
        elif c["kind"] == "image":
            k = add_input(["-loop", "1", "-framerate", fps_s, "-t", f"{d:.4f}", "-i", c["src"]])
            chain = [f"[{k}:v]format=rgba"]
        else:
            k = add_input(["-ss", f"{c['in']:.4f}", "-t", f"{c['out'] - c['in']:.4f}", "-i", c["src"]])
            chain = [f"[{k}:v]setpts=(PTS-STARTPTS)/{c['speed']:.6f}" if c["speed"] != 1 else f"[{k}:v]setpts=PTS-STARTPTS"]
        chain.append(f"fps={fps_s}")
        if c["hflip"]:
            chain.append("hflip")
        mode = _fit_mode(c, W, H) if c["kind"] != "color" else "none"
        zoom = c["zoom"]
        zw, zh = (W * 2, H * 2) if zoom else (W, H)
        pre = ",".join(chain)
        if mode == "blur":
            f.append(f"{pre},split=2[bg{i}][fg{i}]")
            f.append(f"[bg{i}]scale={zw // 8 * 2}:{zh // 8 * 2}:force_original_aspect_ratio=increase,crop={zw // 8 * 2}:{zh // 8 * 2},boxblur=12:2,scale={zw}:{zh},eq=brightness=-0.08[bgb{i}]")
            f.append(f"[fg{i}]scale={zw}:{zh}:force_original_aspect_ratio=decrease[fgs{i}]")
            f.append(f"[bgb{i}][fgs{i}]overlay=(W-w)/2:(H-h)/2[fit{i}]")
        elif mode == "contain":
            f.append(f"{pre},scale={zw}:{zh}:force_original_aspect_ratio=decrease,pad={zw}:{zh}:(ow-iw)/2:(oh-ih)/2:color={bg}[fit{i}]")
        elif mode == "none":
            f.append(f"{pre}[fit{i}]")
        else:
            f.append(f"{pre},scale={zw}:{zh}:force_original_aspect_ratio=increase,crop={zw}:{zh}[fit{i}]")
        post = []
        if zoom:
            if isinstance(zoom, (int, float)):
                zoom = {"from": 1.0, "to": float(zoom)}
            z0, z1 = float(zoom.get("from", 1.0)), float(zoom.get("to", 1.15))
            zx, zy = float(zoom.get("x", 0.5)), float(zoom.get("y", 0.5))
            nfr = max(2, int(round(d * fps)))
            post.append(f"zoompan=z='{z0}+({z1 - z0})*on/{nfr - 1}':x='(iw-iw/zoom)*{zx}':y='(ih-ih/zoom)*{zy}':d=1:s={W}x{H}:fps={fps_s}")
        if c["punch"]:
            pz = float(c["punch"])
            post.append(f"scale={int(W * pz) // 2 * 2}:{int(H * pz) // 2 * 2},crop={W}:{H}")
        post += _color(c["color"]) + grade
        if c.get("lut"):
            lp = _path(c["lut"], P["base"], f"video[{i}].lut")
            local = os.path.join(job, f"lut{i}.cube")
            shutil.copyfile(lp, local)
            post.append(f"lut3d=lut{i}.cube")
        post += ["format=yuv420p", "setsar=1", "settb=AVTB", f"trim=duration={d:.4f}", "setpts=PTS-STARTPTS"]
        f.append(f"[fit{i}]" + ",".join(post) + f"[c{i}]")
        labels.append(f"c{i}")
    # join
    acc, acc_len = labels[0], P["clips"][0]["dur"]
    for n, c in enumerate(P["clips"][1:], 1):
        out = f"j{n}"
        if c["transition"]:
            off = acc_len - c["tdur"]
            f.append(f"[{acc}][{labels[n]}]xfade=transition={c['transition']}:duration={c['tdur']:.4f}:offset={off:.4f}[{out}]")
            acc_len = off + c["dur"]
        else:
            f.append(f"[{acc}][{labels[n]}]concat=n=2:v=1:a=0[{out}]")
            acc_len += c["dur"]
        acc = out
    # overlays
    font_cache = {}
    for n, o in enumerate(P["overlays"]):
        a0, a1 = o["at"], o["at"] + o["dur"]
        fade = float(o.get("fade", 0.2 if o["type"] == "text" else 0))
        out = f"o{n}"
        if o["type"] == "text":
            fp = _font(o.get("font") or ("arialbd" if o.get("bold", True) else "arial"))
            if not fp:
                raise ToolError("no font found for text overlay")
            if fp not in font_cache:
                font_cache[fp] = f"font{len(font_cache)}{os.path.splitext(fp)[1]}"
                shutil.copyfile(fp, os.path.join(job, font_cache[fp]))
            tf = f"text{n}.txt"
            with open(os.path.join(job, tf), "w", encoding="utf-8") as fh:
                fh.write(str(o["text"]))
            size = int(float(o.get("size") or P["H"] / 13) * scale)
            pos = o.get("pos") or o.get("position") or "center"
            xexp = "(w-text_w)/2"
            yexp = {"top": "h*0.08", "center": "(h-text_h)/2", "bottom": "h*0.84-text_h", "lower-third": "h*0.74", "upper-third": "h*0.2"}.get(pos, "(h-text_h)/2")
            if o.get("x") is not None:
                xexp = f"{float(o['x']) * scale:.1f}" if float(o["x"]) > 1 else f"w*{float(o['x'])}-text_w/2"
            if o.get("y") is not None:
                yexp = f"{float(o['y']) * scale:.1f}" if float(o["y"]) > 1 else f"h*{float(o['y'])}-text_h/2"
            alpha = f"if(lt(t,{a0 + fade:.3f}),(t-{a0:.3f})/{fade:.3f},if(gt(t,{a1 - fade:.3f}),({a1:.3f}-t)/{fade:.3f},1))" if fade > 0 else "1"
            opts = [f"fontfile={font_cache[fp]}", f"textfile={tf}", f"fontsize={size}", f"fontcolor={o.get('color') or 'white'}",
                    f"x={xexp}", f"y={yexp}", f"enable='between(t,{a0:.3f},{a1:.3f})'", f"alpha='{alpha}'", "line_spacing=8"]
            stroke = o.get("stroke", max(2, size // 18))
            if stroke:
                opts += [f"borderw={int(stroke)}", f"bordercolor={o.get('stroke_color') or 'black'}"]
            if o.get("shadow", True):
                opts += [f"shadowx={max(1, size // 30)}", f"shadowy={max(1, size // 30)}", "shadowcolor=black@0.55"]
            if o.get("box"):
                opts += ["box=1", f"boxcolor={o.get('box_color') or 'black@0.55'}", f"boxborderw={max(8, size // 3)}"]
            f.append(f"[{acc}]drawtext=" + ":".join(opts) + f"[{out}]")
        else:
            if o["type"] == "image":
                k = add_input(["-loop", "1", "-framerate", fps_s, "-t", f"{o['dur']:.4f}", "-i", o["src"]])
            else:
                k = add_input(["-ss", f"{o['in']:.4f}", "-t", f"{o['dur']:.4f}", "-i", o["src"]])
            ch = [f"[{k}:v]setpts=PTS-STARTPTS", f"fps={fps_s}"]
            ow = o.get("w") or o.get("width")
            if ow is None:
                ch.append(f"scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H}")
                ox, oy = "0", "0"
            else:
                owp = int(float(ow) * (W if float(ow) <= 1 else scale)) // 2 * 2
                ch.append(f"scale={owp}:-2")
                x, y = o.get("x", 0.5), o.get("y", 0.5)
                ox = f"{float(x) * scale:.1f}" if float(x) > 1 else f"(W-w)*{float(x)}"
                oy = f"{float(y) * scale:.1f}" if float(y) > 1 else f"(H-h)*{float(y)}"
            ch.append("format=yuva420p")
            if fade > 0:
                ch += [f"fade=t=in:st=0:d={fade:.3f}:alpha=1", f"fade=t=out:st={max(0, o['dur'] - fade):.3f}:d={fade:.3f}:alpha=1"]
            if o.get("opacity") is not None and float(o["opacity"]) < 1:
                ch.append(f"colorchannelmixer=aa={float(o['opacity']):.3f}")
            ch.append(f"setpts=PTS+{a0:.4f}/TB")
            f.append(",".join(ch) + f"[ov{n}]")
            f.append(f"[{acc}][ov{n}]overlay=x={ox}:y={oy}:enable='between(t,{a0:.3f},{a1:.3f})':eof_action=pass[{out}]")
        acc = out
    cap_info = None
    if P.get("captions"):
        cap_info = build_ass(P, P["captions"], os.path.join(job, "captions.ass"))
        fd = ""
        cf = P["captions"].get("font")
        if cf and os.path.isfile(str(cf)):
            shutil.copyfile(cf, os.path.join(job, os.path.basename(cf)))
            fd = ":fontsdir=."
        f.append(f"[{acc}]subtitles=filename=captions.ass{fd}[capd]")
        acc = "capd"
    f.append(f"[{acc}]trim=duration={P['duration']:.4f},format=yuv420p[vout]")
    return inputs, ";\n".join(f), cap_info, (W, H)


# ------------------------------------------------------------------ audio graph

def _item_lufs(src, start, dur):
    key = (src, round(start, 2), round(dur, 2), os.path.getmtime(src))
    if key not in _lufs_cache:
        lu = A.loudness(src, start or None, min(dur, 120) if dur else None)
        v = lu["lufs"] if lu and lu["lufs"] is not None and lu["lufs"] > -70 else None
        if v is None:  # very short sounds: gate kills integrated loudness -> use true peak as proxy
            v = (lu["true_peak"] - 12) if lu and lu.get("true_peak") is not None else -20.0
        _lufs_cache[key] = v
    return _lufs_cache[key]


def audio_graph(P, job):
    inputs, f = [], []
    L = P["duration"]
    fmt = "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo"

    def add_input(args):
        inputs.append(args)
        return len(inputs) - 1

    # main track audio (dialogue / clip sound)
    labels = []
    for c in P["clips"]:
        i, d = c["i"], c["dur"]
        if c["has_audio"]:
            k = add_input(["-ss", f"{c['in']:.4f}", "-t", f"{c['out'] - c['in']:.4f}", "-i", c["src"]])
            ch = [f"[{k}:a]asetpts=PTS-STARTPTS", fmt] + _atempo(c["speed"])
            if c["volume_db"]:
                ch.append(f"volume={c['volume_db']:.2f}dB")
            ch += [f"afade=t=in:st=0:d=0.012", f"apad=whole_dur={d:.4f}", f"atrim=0:{d:.4f}", f"afade=t=out:st={max(0, d - 0.015):.4f}:d=0.015", "asetpts=PTS-STARTPTS"]
        else:
            k = add_input(["-f", "lavfi", "-t", f"{d:.4f}", "-i", "anullsrc=r=48000:cl=stereo"])
            ch = [f"[{k}:a]{fmt}", f"atrim=0:{d:.4f}", "asetpts=PTS-STARTPTS"]
        f.append(",".join(ch) + f"[ca{i}]")
        labels.append(f"ca{i}")
    acc = labels[0]
    for n, c in enumerate(P["clips"][1:], 1):
        out = f"cj{n}"
        if c["transition"]:
            f.append(f"[{acc}][{labels[n]}]acrossfade=d={c['tdur']:.4f}:c1=tri:c2=tri[{out}]")
        else:
            f.append(f"[{acc}][{labels[n]}]concat=n=2:v=0:a=1[{out}]")
        acc = out
    any_clip_audio = any(c["has_audio"] for c in P["clips"])
    buses = {"voice": [], "music": [], "sfx": [], "ambience": []}
    clip_role = P["clip_role"] if P["clip_role"] in buses else "voice"
    if any_clip_audio:
        # level the dialogue: gentle compression + loudness to the voice target
        lv = _clip_track_lufs(P)
        gain = ROLE_LUFS["voice" if clip_role == "voice" else clip_role] - lv if lv is not None else 0
        gain = max(-20, min(20, gain))
        comp = ",acompressor=threshold=0.1:ratio=3:attack=10:release=150:makeup=1" if clip_role == "voice" else ""
        f.append(f"[{acc}]volume={gain:.2f}dB{comp}[clipbus]")
        buses[clip_role].append("clipbus")
    has_voice = P["has_voice"]
    for a in P["audio"]:
        i = a["i"]
        args = (["-stream_loop", "-1"] if a["loop"] else []) + ["-ss", f"{a['in']:.4f}"] + ([] if a["loop"] else ["-t", f"{a['dur']:.4f}"]) + ["-i", a["src"]]
        k = add_input(args)
        ch = [f"[{k}:a]asetpts=PTS-STARTPTS", fmt, f"atrim=0:{a['dur']:.4f}"]
        g = a["gain"]
        if a["normalize"]:
            target = ROLE_LUFS["bed" if a["role"] == "music" and has_voice else a["role"]]
            g += max(-30, min(24, target - _item_lufs(a["src"], a["in"], a["dur"] if not a["loop"] else 0)))
        ch.append(f"volume={g:.2f}dB")
        if a["pan"]:
            p = max(-1, min(1, a["pan"]))
            ch.append(f"pan=stereo|c0={min(1, 1 - p):.3f}*c0|c1={min(1, 1 + p):.3f}*c1")
        if a["fade_in"] > 0:
            ch.append(f"afade=t=in:st=0:d={a['fade_in']:.3f}")
        if a["fade_out"] > 0:
            ch.append(f"afade=t=out:st={max(0, a['dur'] - a['fade_out']):.3f}:d={a['fade_out']:.3f}")
        ms = int(round(a["at"] * 1000))
        if ms > 0:
            ch.append(f"adelay={ms}:all=1")
        f.append(",".join(ch) + f"[a{i}]")
        buses[a["role"]].append(f"a{i}")
    bus_out = {}
    for role, ls in buses.items():
        if not ls:
            continue
        if len(ls) == 1:
            f.append(f"[{ls[0]}]anull[bus_{role}]")
        else:
            f.append("".join(f"[{x}]" for x in ls) + f"amix=inputs={len(ls)}:normalize=0:dropout_transition=0:duration=longest[bus_{role}]")
        bus_out[role] = f"bus_{role}"
    # duck music + ambience under voice
    if P["duck"] and "voice" in bus_out and ("music" in bus_out or "ambience" in bus_out):
        beds = [r for r in ("music", "ambience") if r in bus_out]
        f.append(f"[{bus_out['voice']}]asplit={len(beds) + 1}[vmain]" + "".join(f"[vsc{r}]" for r in beds))
        bus_out["voice"] = "vmain"
        ratio = max(2, min(20, P["duck_db"] / 1.5))
        for r in beds:
            f.append(f"[{bus_out[r]}][vsc{r}]sidechaincompress=threshold=0.015:ratio={ratio:.1f}:attack=25:release=450:knee=3:makeup=1[duck_{r}]")
            bus_out[r] = f"duck_{r}"
    outs = list(bus_out.values())
    if not outs:
        k = add_input(["-f", "lavfi", "-t", f"{L:.4f}", "-i", "anullsrc=r=48000:cl=stereo"])
        outs = [f"{k}:a"]
    if len(outs) == 1:
        f.append(f"[{outs[0]}]anull[mixed]")
    else:
        f.append("".join(f"[{x}]" for x in outs) + f"amix=inputs={len(outs)}:normalize=0:dropout_transition=0:duration=longest[mixed]")
    f.append(f"[mixed]apad=whole_dur={L:.4f},atrim=0:{L:.4f},alimiter=limit=0.95:level=0[aout]")
    return inputs, ";\n".join(f)


def _clip_track_lufs(P):
    vals = []
    for c in P["clips"]:
        if c["has_audio"]:
            v = _item_lufs(c["src"], c["in"], c["out"] - c["in"])
            if v is not None:
                vals.append((v, c["dur"]))
    if not vals:
        return None
    import math
    tot = sum(d for _, d in vals)
    return 10 * math.log10(sum(10 ** (v / 10) * d for v, d in vals) / tot)


# ------------------------------------------------------------------ running ffmpeg

def _ff(argv, cwd, total, progress, label, cancel=None):
    argv = [A.which("ffmpeg"), "-hide_banner", "-y", "-nostats", "-progress", "pipe:1"] + argv
    errlog = os.path.join(cwd, f"{label}.log")
    try:
        with open(os.path.join(cwd, f"{label}.cmd.json"), "w", encoding="utf-8") as fh:
            json.dump(argv, fh)
    except OSError:
        pass
    with open(errlog, "w", encoding="utf-8") as ef:
        proc = subprocess.Popen(argv, cwd=cwd, stdout=subprocess.PIPE, stderr=ef, stdin=subprocess.DEVNULL,
                                creationflags=A.NOWIN, text=True, encoding="utf-8", errors="replace")
        last = 0
        for line in proc.stdout:
            if cancel and cancel():
                proc.kill()
                raise ToolError("cancelled")
            if line.startswith("out_time_us=") and progress:
                try:
                    t = int(line.split("=")[1]) / 1e6
                except ValueError:
                    continue
                if time.time() - last > 1.5:
                    last = time.time()
                    progress(f"{label}: {A.fmt_t(t)} / {A.fmt_t(total)}", min(t, total), total)
        proc.wait()
    if proc.returncode != 0:
        tail = open(errlog, encoding="utf-8", errors="replace").read().strip().splitlines()[-15:]
        raise ToolError(f"ffmpeg {label} failed ({proc.returncode}). Log: {errlog}\n" + "\n".join(tail))


def nvenc_ok():
    global _nvenc
    if _nvenc is None:
        r = A.run([A.which("ffmpeg"), "-v", "error", "-f", "lavfi", "-i", "color=s=256x256:d=0.2", "-c:v", "h264_nvenc", "-f", "null", "-"], check=False, timeout=30)
        _nvenc = r.returncode == 0
    return _nvenc


def _vcodec(quality, encoder):
    enc = (encoder or "auto").lower()
    if quality == "preview":
        if enc in ("auto", "nvenc") and nvenc_ok():
            return ["-c:v", "h264_nvenc", "-preset", "p2", "-cq", "30"]
        return ["-c:v", "libx264", "-preset", "ultrafast", "-crf", "30"]
    if enc == "nvenc" or (enc == "auto" and nvenc_ok()):
        return ["-c:v", "h264_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", "19", "-b:v", "0", "-profile:v", "high"]
    return ["-c:v", "libx264", "-preset", "medium", "-crf", "18", "-profile:v", "high"]


def render(tl, base=None, preview=False, encoder="auto", progress=None, cancel=None, output=None):
    P = plan(tl, base)
    quality = "preview" if preview else "final"
    stamp = time.strftime("%Y%m%d-%H%M%S")
    job = os.path.join(A.OUT, "renders", stamp + ("-preview" if preview else ""))
    os.makedirs(job, exist_ok=True)
    out = output or P["output"] or f"montage-{stamp}.mp4"
    if not os.path.isabs(out):
        out = os.path.join(P["base"] or A.OUT, out) if P["base"] else os.path.join(A.OUT, out)
    if preview:
        root, ext = os.path.splitext(out)
        out = root + ".preview" + (ext or ".mp4")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    L = P["duration"]
    t0 = time.time()
    with open(os.path.join(job, "timeline.json"), "w", encoding="utf-8") as fh:
        json.dump(P["raw"], fh, ensure_ascii=False, indent=1)
    # 1) audio mix
    ain, ag = audio_graph(P, job)
    with open(os.path.join(job, "audio.graph"), "w", encoding="utf-8") as fh:
        fh.write(ag)
    _ff([x for i in ain for x in i] + ["-/filter_complex", "audio.graph", "-map", "[aout]", "-c:a", "pcm_f32le", "mix.wav"], job, L, progress, "audio", cancel)
    # 2) video
    scale = 1.0
    if preview:
        scale = min(1.0, 640 / max(P["W"], P["H"]) * (16 / 9 if P["H"] > P["W"] else 1))
    vin, vg, cap, (W, H) = video_graph(P, job, scale)
    with open(os.path.join(job, "video.graph"), "w", encoding="utf-8") as fh:
        fh.write(vg)
    _ff([x for i in vin for x in i] + ["-/filter_complex", "video.graph", "-map", "[vout]", "-an"] + _vcodec(quality, encoder) +
        ["-pix_fmt", "yuv420p", "-r", f"{P['fps']:g}", "video.mp4"], job, L, progress, "video", cancel)
    # 3) loudness: static gain to the target + peak limiter, re-measured (limiting can eat loudness) + mux
    I, TP = P["lufs"], P["true_peak"]
    lim = max(0.0625, min(1.0, 10 ** ((TP - 0.5) / 20)))  # sample-peak ceiling a bit under the true-peak target
    master = "mix.wav"
    lu = A.loudness(os.path.join(job, "mix.wav"))
    if lu and lu.get("lufs") is not None and lu["lufs"] > -70:
        gain = I - lu["lufs"]
        for _ in range(3):
            _ff(["-i", "mix.wav", "-af", f"volume={gain:.3f}dB,alimiter=limit={lim:.4f}:level=0:attack=2:release=60", "-c:a", "pcm_f32le", "master.wav"],
                job, L, progress, "loudness", cancel)
            m = A.loudness(os.path.join(job, "master.wav"))
            if not m or m.get("lufs") is None:
                break
            err = I - m["lufs"]
            if abs(err) <= 0.3:
                break
            gain += err
        master = "master.wav"
    _ff(["-i", "video.mp4", "-i", master, "-map", "0:v", "-map", "1:a", "-c:v", "copy", "-af", "aresample=48000", "-c:a", "aac", "-b:a", "256k" if not preview else "128k",
         "-movflags", "+faststart", "-shortest", out], job, L, progress, "mux", cancel)
    final_lu = A.loudness(out)
    credits = write_credits(P, os.path.splitext(out)[0] + ".CREDITS.txt")
    res = summary(P)
    res.update({"output": out, "render_seconds": round(time.time() - t0, 1), "quality": quality, "resolution": [W, H],
                "loudness": {"lufs": final_lu and final_lu["lufs"], "true_peak": final_lu and final_lu["true_peak"], "target": I},
                "job_dir": job, "captions": cap})
    if credits:
        res["credits_file"] = credits
    return res


def write_credits(P, path):
    lines, seen = [], set()
    for a in P["audio"]:
        rec = S.record_for_path(a["src"])
        if not rec or rec.get("source") == "local" or rec["path"] in seen:
            continue
        seen.add(rec["path"])
        lic = rec.get("license", "")
        lines.append(rec.get("attribution") or f'"{rec.get("title")}" by {rec.get("author")} - {S.LICENSES.get(lic, lic)}')
        if rec.get("page"):
            lines[-1] += f" <{rec['page']}>"
    if not lines:
        return None
    with open(path, "w", encoding="utf-8") as f:
        f.write("Sound & music credits (paste into the video description):\n" + "\n".join(lines) + "\n")
    return path


# ------------------------------------------------------------------ OTIO export

def export_otio(tl, base=None, output=None):
    try:
        import opentimelineio as otio
    except ImportError:
        raise ToolError("export needs opentimelineio: python -m pip install opentimelineio")
    P = plan(tl, base)
    fps = P["fps"]
    RT, TR = otio.opentime.RationalTime, otio.opentime.TimeRange
    rt = lambda s: RT(round(s * fps), fps)
    T = otio.schema.Timeline(name=os.path.splitext(os.path.basename(output or "montage"))[0])
    T.global_start_time = RT(0, fps)
    v = otio.schema.Track(name="V1", kind=otio.schema.TrackKind.Video)
    a1 = otio.schema.Track(name="A1 clip audio", kind=otio.schema.TrackKind.Audio)
    for n, c in enumerate(P["clips"]):
        if c["transition"] and n:
            half = rt(c["tdur"] / 2)
            v.append(otio.schema.Transition(transition_type=otio.schema.TransitionTypes.SMPTE_Dissolve, in_offset=half, out_offset=half))
        if c["kind"] == "color":
            v.append(otio.schema.Gap(source_range=TR(RT(0, fps), rt(c["dur"]))))
            a1.append(otio.schema.Gap(source_range=TR(RT(0, fps), rt(c["dur"]))))
            continue
        ref = otio.schema.ExternalReference(target_url="file:///" + c["src"].replace("\\", "/"))
        dur_src = (c["out"] - c["in"]) if c["kind"] == "video" else c["dur"]
        clip = otio.schema.Clip(name=c["label"], media_reference=ref, source_range=TR(rt(c["in"]), rt(dur_src)))
        if c["speed"] != 1:
            clip.effects.append(otio.schema.LinearTimeWarp(time_scalar=c["speed"]))
        v.append(clip)
        if c["has_audio"]:
            ac = otio.schema.Clip(name=c["label"], media_reference=otio.schema.ExternalReference(target_url=ref.target_url), source_range=TR(rt(c["in"]), rt(dur_src)))
            a1.append(ac)
        else:
            a1.append(otio.schema.Gap(source_range=TR(RT(0, fps), rt(c["dur"]))))
    T.tracks.append(v)
    T.tracks.append(a1)
    # one audio track per role, items laid out with gaps; overlapping items spill to extra tracks
    for role in ("voice", "music", "sfx", "ambience"):
        lanes = []
        for a in sorted((x for x in P["audio"] if x["role"] == role), key=lambda x: x["at"]):
            for lane in lanes:
                if lane["end"] <= a["at"] + 1e-6:
                    break
            else:
                lane = {"track": otio.schema.Track(name=f"{role} {len(lanes) + 1}", kind=otio.schema.TrackKind.Audio), "end": 0.0}
                lanes.append(lane)
            gap = a["at"] - lane["end"]
            if gap > 1 / fps:
                lane["track"].append(otio.schema.Gap(source_range=TR(RT(0, fps), rt(gap))))
            lane["track"].append(otio.schema.Clip(name=a["label"], media_reference=otio.schema.ExternalReference(target_url="file:///" + a["src"].replace("\\", "/")),
                                                  source_range=TR(rt(a["in"]), rt(a["dur"]))))
            lane["end"] = a["at"] + a["dur"]
        for lane in lanes:
            T.tracks.append(lane["track"])
    out = output or os.path.join(A.OUT, f"montage-{time.strftime('%Y%m%d-%H%M%S')}.otio")
    if not os.path.isabs(out):
        out = os.path.join(P["base"] or A.OUT, out)
    otio.adapters.write_to_file(T, out)
    return {"output": out, "tracks": len(T.tracks), "duration": round(P["duration"], 3),
            "note": "DaVinci Resolve: File > Import > Timeline (.otio). Premiere Pro 25+: File > Import (OTIO). Kdenlive: File > Import > OpenTimelineIO."}
