"""Look and listen: probe, contact sheets, scenes, silences, loudness, beats, transcription."""

import json
import math
import os
import re
import shutil
import subprocess
import tempfile

from nerve_plugin import ToolError

OUT = os.path.join(os.environ.get("NERVE_OUT") or tempfile.gettempdir(), "montage")
CACHE = os.path.join(OUT, "cache")
os.makedirs(CACHE, exist_ok=True)
NOWIN = 0x08000000 if os.name == "nt" else 0


def which(name):
    env = os.environ.get(name.upper() + "_PATH", "").strip()
    if env and os.path.isfile(env):
        return env
    w = shutil.which(name)
    if not w:
        raise ToolError(f"{name} not found. Install FFmpeg (winget install Gyan.FFmpeg) or set {name.upper()}_PATH in plugin.toml [env].")
    return w


def run(argv, timeout=None, check=True, cwd=None):
    r = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout,
                       stdin=subprocess.DEVNULL, creationflags=NOWIN, cwd=cwd)
    if check and r.returncode != 0:
        tail = "\n".join((r.stderr or "").strip().splitlines()[-12:])
        raise ToolError(f"{os.path.basename(argv[0])} failed ({r.returncode}):\n{tail}")
    return r


def need(path):
    if not path:
        raise ToolError("`path` is required")
    p = os.path.abspath(os.path.expandvars(os.path.expanduser(str(path))))
    if not os.path.isfile(p):
        raise ToolError(f"file not found: {p}")
    return p


_probe_cache = {}


def probe(path):
    path = need(path)
    key = (path, os.path.getmtime(path))
    if key in _probe_cache:
        return _probe_cache[key]
    r = run([which("ffprobe"), "-v", "error", "-print_format", "json", "-show_format", "-show_streams", path])
    j = json.loads(r.stdout or "{}")
    v = next((s for s in j.get("streams", []) if s.get("codec_type") == "video" and not (s.get("disposition") or {}).get("attached_pic")), None)
    a = next((s for s in j.get("streams", []) if s.get("codec_type") == "audio"), None)
    fmt = j.get("format", {})
    dur = float(fmt.get("duration") or (v or a or {}).get("duration") or 0)
    info = {"path": path, "duration": round(dur, 3), "size_mb": round(int(fmt.get("size") or 0) / 1e6, 2), "format": fmt.get("format_name")}
    ext = os.path.splitext(path)[1].lower()
    is_image = ext in (".png", ".jpg", ".jpeg", ".webp", ".bmp", ".gif", ".tif", ".tiff") and ext != ".gif"
    if v:
        fr = v.get("avg_frame_rate") or v.get("r_frame_rate") or "0/1"
        n, d = (fr.split("/") + ["1"])[:2]
        fps = float(n) / float(d) if float(d or 0) else 0
        rot = 0
        for sd in v.get("side_data_list") or []:
            if "rotation" in sd:
                rot = int(sd["rotation"])
        w, h = v.get("width"), v.get("height")
        if abs(rot) in (90, 270):
            w, h = h, w
        info["video"] = {"codec": v.get("codec_name"), "width": w, "height": h, "fps": round(fps, 3), "pix_fmt": v.get("pix_fmt")}
    if a:
        info["audio"] = {"codec": a.get("codec_name"), "sample_rate": int(a.get("sample_rate") or 0), "channels": a.get("channels")}
    info["image"] = bool(is_image or (v and dur == 0 and not a))
    _probe_cache[key] = info
    return info


def fmt_t(t):
    t = max(0.0, float(t))
    m, s = divmod(t, 60)
    return f"{int(m)}:{s:05.2f}" if m else f"{s:.2f}s"


# ------------------------------------------------------------------ eyes

def frame(path, t, out=None, width=960):
    path = need(path)
    out = out or os.path.join(CACHE, f"frame_{abs(hash((path, t))) % 10**8}.jpg")
    run([which("ffmpeg"), "-y", "-v", "error", "-ss", f"{float(t):.3f}", "-i", path, "-frames:v", "1",
         "-vf", f"scale='min({width},iw)':-2", "-q:v", "3", out])
    if not os.path.exists(out):
        raise ToolError(f"no frame at {t}s (duration {probe(path)['duration']}s)")
    return out


def contact_sheet(path, count=12, start=None, end=None, cols=None, width=1600, times=None):
    """Grid of frames with timestamps -> jpg path, times."""
    from PIL import Image, ImageDraw, ImageFont

    info = probe(path)
    dur = info["duration"]
    if info.get("image"):
        return need(path), [0.0]
    s = max(0.0, float(start or 0))
    e = min(dur, float(end)) if end else dur
    if times is None:
        count = max(1, min(int(count or 12), 48))
        step = (e - s) / count
        times = [s + step * (i + 0.5) for i in range(count)]
    cols = int(cols or (4 if len(times) > 6 else len(times)))
    rows = math.ceil(len(times) / cols)
    tw = max(160, width // cols)
    tmpdir = tempfile.mkdtemp(dir=CACHE)
    tiles = []
    try:
        for i, t in enumerate(times):
            fp = os.path.join(tmpdir, f"{i}.jpg")
            run([which("ffmpeg"), "-y", "-v", "error", "-ss", f"{t:.3f}", "-i", info["path"], "-frames:v", "1",
                 "-vf", f"scale={tw}:-2", "-q:v", "4", fp], check=False)
            tiles.append(Image.open(fp).convert("RGB") if os.path.exists(fp) else Image.new("RGB", (tw, tw * 9 // 16)))
        th = max(im.height for im in tiles)
        sheet = Image.new("RGB", (cols * tw, rows * th), (16, 16, 16))
        d = ImageDraw.Draw(sheet)
        try:
            font = ImageFont.truetype("arialbd.ttf", max(14, tw // 14))
        except Exception:
            font = ImageFont.load_default()
        for i, (im, t) in enumerate(zip(tiles, times)):
            x, y = (i % cols) * tw, (i // cols) * th
            sheet.paste(im, (x, y))
            label = fmt_t(t)
            bb = d.textbbox((0, 0), label, font=font)
            d.rectangle([x + 4, y + 4, x + 12 + bb[2], y + 10 + bb[3]], fill=(0, 0, 0))
            d.text((x + 8, y + 6), label, fill=(255, 230, 0), font=font)
        out = os.path.join(CACHE, f"sheet_{abs(hash((info['path'], tuple(round(t, 2) for t in times)))) % 10**8}.jpg")
        sheet.save(out, quality=82)
        return out, times
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


def scenes(path, threshold=0.3, min_gap=0.4):
    path = need(path)
    r = run([which("ffmpeg"), "-hide_banner", "-nostats", "-i", path, "-an", "-sn",
             "-vf", f"scale=320:-2,select='gt(scene,{float(threshold)})',showinfo", "-f", "null", "-"], check=False)
    cuts = []
    for m in re.finditer(r"pts_time:([\d.]+)", r.stderr or ""):
        t = float(m.group(1))
        if not cuts or t - cuts[-1] >= min_gap:
            cuts.append(round(t, 3))
    dur = probe(path)["duration"]
    bounds = [0.0] + cuts + [dur]
    shots = [{"in": bounds[i], "out": bounds[i + 1], "len": round(bounds[i + 1] - bounds[i], 2)} for i in range(len(bounds) - 1) if bounds[i + 1] - bounds[i] > 0.05]
    return {"cuts": cuts, "shots": shots, "duration": dur}


def silences(path, noise_db=-35, min_silence=0.35, pad=0.08, min_keep=0.25):
    path = need(path)
    r = run([which("ffmpeg"), "-hide_banner", "-nostats", "-i", path, "-vn", "-sn",
             "-af", f"silencedetect=noise={noise_db}dB:d={min_silence}", "-f", "null", "-"], check=False)
    dur = probe(path)["duration"]
    sil, cur = [], None
    for line in (r.stderr or "").splitlines():
        m = re.search(r"silence_start: (-?[\d.]+)", line)
        if m:
            cur = max(0.0, float(m.group(1)))
        m = re.search(r"silence_end: ([\d.]+)", line)
        if m and cur is not None:
            sil.append((cur, float(m.group(1))))
            cur = None
    if cur is not None:
        sil.append((cur, dur))
    keep, t = [], 0.0
    for a, b in sil:
        if a - t > 0:
            keep.append([max(0.0, t - pad), min(dur, a + pad)])
        t = b
    if dur - t > 0:
        keep.append([max(0.0, t - pad), dur])
    merged = []
    for a, b in keep:
        if merged and a <= merged[-1][1]:
            merged[-1][1] = b
        else:
            merged.append([a, b])
    merged = [[round(a, 3), round(b, 3)] for a, b in merged if b - a >= min_keep]
    kept = sum(b - a for a, b in merged)
    return {"duration": dur, "silences": [[round(a, 3), round(b, 3)] for a, b in sil], "keep": merged,
            "kept_seconds": round(kept, 2), "removed_seconds": round(dur - kept, 2)}


def loudness(path, start=None, dur=None):
    path = need(path)
    argv = [which("ffmpeg"), "-hide_banner", "-nostats"]
    if start:
        argv += ["-ss", str(start)]
    if dur:
        argv += ["-t", str(dur)]
    argv += ["-i", path, "-vn", "-af", "loudnorm=print_format=json", "-f", "null", "-"]
    r = run(argv, check=False)
    m = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", r.stderr or "", re.S)
    if not m:
        return None
    j = json.loads(m.group(0))
    f = lambda k: None if j.get(k) in (None, "-inf", "inf") else round(float(j[k]), 2)
    return {"lufs": f("input_i"), "true_peak": f("input_tp"), "lra": f("input_lra"), "threshold": f("input_thresh"), "_raw": j}


# ------------------------------------------------------------------ ears

def decode_mono(path, sr=22050, start=None, dur=None):
    import numpy as np

    argv = [which("ffmpeg"), "-v", "error"]
    if start:
        argv += ["-ss", str(start)]
    if dur:
        argv += ["-t", str(dur)]
    argv += ["-i", need(path), "-vn", "-ac", "1", "-ar", str(sr), "-f", "f32le", "-"]
    r = subprocess.run(argv, capture_output=True, stdin=subprocess.DEVNULL, creationflags=NOWIN)
    if r.returncode != 0:
        raise ToolError("could not decode audio: " + r.stderr.decode("utf-8", "replace")[-400:])
    return np.frombuffer(r.stdout, dtype=np.float32), sr


def inspect_audio(path, image=True):
    """Describe a sound so you can place it without hearing it."""
    import numpy as np

    y, sr = decode_mono(path)
    if y.size == 0:
        raise ToolError("no audio")
    dur = y.size / sr
    hop = 256
    n = y.size // hop
    env = np.sqrt(np.mean(y[: n * hop].reshape(n, hop) ** 2, axis=1) + 1e-12)
    db = 20 * np.log10(env + 1e-9)
    pk = int(np.argmax(env))
    peak_t = pk * hop / sr
    pk_db = float(db[pk])
    above = np.where(db >= pk_db - 6)[0]
    attack = (pk - (above[0] if above.size else pk)) * hop / sr
    loud = np.where(db >= pk_db - 30)[0]
    start_t = (loud[0] * hop / sr) if loud.size else 0.0
    end_t = (loud[-1] * hop / sr) if loud.size else dur
    spec = np.abs(np.fft.rfft(y[: min(y.size, sr * 10)] * np.hanning(min(y.size, sr * 10))))
    freqs = np.fft.rfftfreq(min(y.size, sr * 10), 1 / sr)
    centroid = float((spec * freqs).sum() / (spec.sum() + 1e-9))
    shape = "impact/hit (sharp attack)" if attack < 0.03 and end_t - peak_t < 1.5 else \
        "riser/build-up (swells to the end)" if peak_t > 0.7 * dur and attack > 0.4 else \
        "whoosh/swell (rises then falls)" if 0.08 <= attack and end_t - peak_t > 0.05 and dur < 4 else \
        "sustained/bed" if end_t - start_t > 4 else "short one-shot"
    res = {
        "path": os.path.abspath(path), "duration": round(dur, 3), "peak_time": round(peak_t, 3),
        "attack": round(attack, 3), "audible": [round(start_t, 3), round(end_t, 3)],
        "tail_after_peak": round(end_t - peak_t, 3), "brightness_hz": int(centroid),
        "tone": "bright/airy" if centroid > 3500 else "mid" if centroid > 1200 else "dark/boomy",
        "shape": shape,
        "hint": f"to land the peak on a cut at time T, start this at T-{peak_t:.2f} (or use align:'peak' in render)",
    }
    lu = loudness(path)
    if lu:
        res["lufs"], res["true_peak"] = lu["lufs"], lu["true_peak"]
    if image:
        out = os.path.join(CACHE, f"wave_{abs(hash(os.path.abspath(path))) % 10**8}.png")
        run([which("ffmpeg"), "-y", "-v", "error", "-i", need(path), "-filter_complex",
             "aformat=channel_layouts=mono,compand=gain=2,showwavespic=s=1000x200:colors=0x33ccff", "-frames:v", "1", out], check=False)
        res["waveform"] = out if os.path.exists(out) else None
    return res


def beats(path, start=None, dur=None):
    try:
        import librosa
        import numpy as np
    except ImportError:
        raise ToolError("beat analysis needs librosa: python -m pip install librosa soundfile (or montage {action:'setup'})")
    y, sr = decode_mono(path, 22050, start, dur)
    if y.size < sr:
        raise ToolError("audio too short for beat tracking")
    hop = 512
    onset = librosa.onset.onset_strength(y=y, sr=sr, hop_length=hop)
    tempo, bf = librosa.beat.beat_track(onset_envelope=onset, sr=sr, hop_length=hop)
    tempo = float(np.atleast_1d(tempo)[0])
    bt = librosa.frames_to_time(bf, sr=sr, hop_length=hop)
    # downbeats: the beat phase (of 4) with the strongest onsets
    down = []
    if len(bf) >= 8:
        strength = [float(onset[bf[k::4]].mean()) for k in range(4)]
        ph = int(np.argmax(strength))
        down = bt[ph::4]
    # energy curve + hits (biggest jumps in loudness = drops / accents)
    rms = librosa.feature.rms(y=y, hop_length=hop)[0]
    rt = librosa.frames_to_time(np.arange(rms.size), sr=sr, hop_length=hop)
    db = 20 * np.log10(rms + 1e-6)
    k = max(1, int(1.0 * sr / hop))
    sm = np.convolve(db, np.ones(k) / k, mode="same")
    jump = np.zeros_like(sm)
    jump[k:] = sm[k:] - sm[:-k]
    hits = []
    for i in np.argsort(-jump):
        if jump[i] < 4 or len(hits) >= 8:
            break
        t = float(rt[i])
        if all(abs(t - h) > 4 for h in hits):
            hits.append(t)
    hits.sort()
    # snap hits to nearest beat
    if len(bt):
        hits = [float(bt[np.argmin(np.abs(bt - h))]) for h in hits]
    # sections
    sections = []
    try:
        mf = librosa.feature.mfcc(y=y, sr=sr, hop_length=hop * 4, n_mfcc=13)
        total = y.size / sr
        nseg = int(min(10, max(3, total // 20)))
        bnd = librosa.segment.agglomerative(mf, nseg)
        bts = sorted(set([0.0] + [float(x) for x in librosa.frames_to_time(bnd, sr=sr, hop_length=hop * 4)] + [total]))
        for a, b in zip(bts, bts[1:]):
            if b - a < 1:
                continue
            m = (rt >= a) & (rt < b)
            sections.append({"start": round(a, 2), "end": round(b, 2), "energy": round(float(rms[m].mean() / (rms.max() + 1e-9)), 2) if m.any() else 0})
    except Exception:
        pass
    # energy per second (0..1), compact
    per = []
    tot = int(y.size / sr)
    for s in range(tot):
        m = (rt >= s) & (rt < s + 1)
        per.append(round(float(rms[m].mean() / (rms.max() + 1e-9)), 2) if m.any() else 0)
    off = float(start or 0)
    r3 = lambda a: [round(float(x) + off, 3) for x in a]
    return {"path": need(path), "tempo_bpm": round(tempo, 1), "beat_interval": round(60 / tempo, 3) if tempo else None,
            "beats": r3(bt), "downbeats": r3(down), "hits": r3(hits), "sections": [dict(s, start=round(s["start"] + off, 2), end=round(s["end"] + off, 2)) for s in sections],
            "energy_per_sec": per, "duration": round(y.size / sr + 0, 2)}


_whisper = {}


def transcribe(path, language=None, model=None, device=None, progress=None):
    try:
        from faster_whisper import WhisperModel
    except ImportError:
        raise ToolError("transcription needs faster-whisper: python -m pip install faster-whisper")
    path = need(path)
    model = model or os.environ.get("WHISPER_MODEL", "small")
    device = device or os.environ.get("WHISPER_DEVICE", "auto")
    key = (model, device)
    if key not in _whisper:
        errs = []
        for dev, ct in ([("cuda", "float16"), ("cpu", "int8")] if device == "auto" else [(device, "float16" if device == "cuda" else "int8")]):
            try:
                if progress:
                    progress(f"loading whisper {model} on {dev}")
                m = WhisperModel(model, device=dev, compute_type=ct)
                if dev == "cuda":  # smoke test: missing cuDNN/cuBLAS only fails at first use
                    import numpy as np
                    list(m.transcribe(np.zeros(16000, dtype=np.float32), language="en")[0])
                _whisper[key] = (m, dev)
                break
            except Exception as e:
                errs.append(f"{dev}: {e}")
        if key not in _whisper:
            raise ToolError("could not load whisper: " + " | ".join(errs))
    m, dev = _whisper[key]
    # decode with our ffmpeg (PyAV in some faster-whisper builds breaks on newer av versions)
    audio, _sr = decode_mono(path, 16000)
    segs, info = m.transcribe(audio, language=language, word_timestamps=True, vad_filter=True, beam_size=5)
    out_segs, words = [], []
    total = info.duration or 1
    for s in segs:
        ws = [{"word": w.word.strip(), "start": round(w.start, 3), "end": round(w.end, 3)} for w in (s.words or []) if w.word.strip()]
        words += ws
        out_segs.append({"start": round(s.start, 3), "end": round(s.end, 3), "text": s.text.strip()})
        if progress:
            progress(f"transcribed {fmt_t(s.end)} / {fmt_t(total)}", s.end, total)
    res = {"source": path, "language": info.language, "duration": round(info.duration, 3), "model": model, "device": dev,
           "segments": out_segs, "words": words}
    base = os.path.join(OUT, os.path.splitext(os.path.basename(path))[0])
    with open(base + ".transcript.json", "w", encoding="utf-8") as f:
        json.dump(res, f, ensure_ascii=False, indent=0)
    with open(base + ".srt", "w", encoding="utf-8") as f:
        for i, s in enumerate(out_segs, 1):
            f.write(f"{i}\n{_srt(s['start'])} --> {_srt(s['end'])}\n{s['text']}\n\n")
    res["files"] = {"json": base + ".transcript.json", "srt": base + ".srt"}
    return res


def _srt(t):
    ms = int(round(t * 1000))
    h, ms = divmod(ms, 3600000)
    m, ms = divmod(ms, 60000)
    s, ms = divmod(ms, 1000)
    return f"{h:02}:{m:02}:{s:02},{ms:03}"
