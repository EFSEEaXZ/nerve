"""Real sound effects and music: Openverse (Freesound + Jamendo, no key), Freesound API (optional token), local library."""

import hashlib
import json
import os
import re
import threading
import time
import urllib.parse
import urllib.request

from nerve_plugin import ToolError

DATA = os.environ.get("NERVE_DATA") or os.path.join(os.path.expanduser("~"), ".nerve")
MEDIA = os.environ.get("NERVE_MEDIA_DIR", "").strip() or os.path.join(DATA, "media")
INDEX = os.path.join(MEDIA, "library.json")
AUDIO_EXT = {".wav", ".mp3", ".ogg", ".flac", ".aif", ".aiff", ".m4a", ".opus", ".wma"}
UA = {"User-Agent": "nerve-montage/1.0 (+https://github.com/EFSEEaXZ/nerve)"}
_lock = threading.RLock()

# Openverse license codes -> human; "safe" = commercial use + may be synced/edited into a video.
LICENSES = {
    "cc0": "CC0 (public domain)", "pdm": "Public Domain Mark", "by": "CC BY 4.0 (credit the author)",
    "by-sa": "CC BY-SA (credit + share-alike)", "by-nc": "CC BY-NC (NON-commercial only)",
    "by-nd": "CC BY-ND (no edits/sync)", "by-nc-sa": "CC BY-NC-SA (non-commercial)", "by-nc-nd": "CC BY-NC-ND (non-commercial, no edits)",
    "sampling+": "Sampling+", "nc-sampling+": "NC Sampling+ (non-commercial)",
}
SAFE = {"cc0", "pdm", "by", "by-sa"}


def media_dirs():
    extra = [x.strip() for x in re.split(r"[;\n]", os.environ.get("NERVE_MEDIA_DIRS", "")) if x.strip()]
    return [MEDIA] + [d for d in extra if os.path.isdir(d)]


def _get_json(url, headers=None, timeout=25):
    req = urllib.request.Request(url, headers=dict(UA, **(headers or {})))
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        body = e.read()[:300].decode("utf-8", "replace")
        raise ToolError(f"{url.split('?')[0]} -> HTTP {e.code}: {body}")


def _slug(s, n=48):
    s = re.sub(r"[^\w\-]+", "_", str(s), flags=re.U).strip("_")
    return (s[:n] or "sound").lower()


def _lic_norm(lic):
    lic = (lic or "").lower().strip()
    m = {"creative commons 0": "cc0", "attribution": "by", "attribution noncommercial": "by-nc",
         "attribution non-commercial": "by-nc", "sampling+": "sampling+"}
    if lic.startswith("http"):
        if "/zero/" in lic:
            return "cc0"
        mm = re.search(r"licenses/([a-z\-+]+)/", lic)
        return mm.group(1) if mm else lic
    return m.get(lic, lic)


# ------------------------------------------------------------------ index (downloaded + local folders)

def load_index():
    try:
        with open(INDEX, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return {"items": {}, "scanned": 0}


def save_index(idx):
    os.makedirs(MEDIA, exist_ok=True)
    tmp = INDEX + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(idx, f, ensure_ascii=False, indent=0)
    os.replace(tmp, INDEX)


def _local_id(path):
    return "local:" + hashlib.sha1(os.path.abspath(path).lower().encode()).hexdigest()[:12]


def scan(progress=None, probe=None):
    """Index audio files under the media dirs (names/folders become search tags)."""
    with _lock:
        idx = load_index()
        items = idx["items"]
        seen = set()
        n_new = 0
        for root in media_dirs():
            for dp, dirs, files in os.walk(root):
                dirs[:] = [d for d in dirs if not d.startswith(".") and d not in ("cache", "renders")]
                for f in files:
                    if os.path.splitext(f)[1].lower() not in AUDIO_EXT:
                        continue
                    path = os.path.join(dp, f)
                    known = next((k for k, v in items.items() if v.get("path") == path), None)
                    if known:
                        seen.add(known)
                        continue
                    iid = _local_id(path)
                    rel = os.path.relpath(path, root)
                    kind = "music" if re.search(r"(?i)music|song|track|bgm|soundtrack", rel) else "sfx"
                    items[iid] = {"id": iid, "path": path, "title": os.path.splitext(f)[0], "source": "local",
                                  "tags": re.findall(r"[^\W_]+", os.path.splitext(rel)[0].lower(), re.U)[:24],
                                  "license": "local", "kind": kind}
                    seen.add(iid)
                    n_new += 1
                    if progress and n_new % 200 == 0:
                        progress(f"indexed {n_new} new files")
        for k in [k for k, v in items.items() if v.get("path") and not os.path.exists(v["path"])]:
            items.pop(k, None)
        idx["scanned"] = time.time()
        save_index(idx)
        return n_new, len(items)


def _tok(s):
    return [t[:6] for t in re.findall(r"[^\W_]+", str(s).lower(), re.U) if len(t) > 1]


def search_local(query, kind=None, min_d=None, max_d=None, limit=10):
    idx = load_index()
    if not idx["items"] or time.time() - idx.get("scanned", 0) > 3600:
        scan()
        idx = load_index()
    q = set(_tok(query))
    res = []
    for it in idx["items"].values():
        if kind and it.get("kind") and it["kind"] != kind:
            continue
        d = it.get("duration")
        if d is not None and ((min_d and d < min_d) or (max_d and d > max_d)):
            continue
        hay = _tok(it.get("title", "")) + [t[:6] for t in it.get("tags", [])]
        score = sum(1 for t in q if any(h.startswith(t) or t.startswith(h) for h in hay))
        if score:
            res.append((score, it))
    res.sort(key=lambda x: -x[0])
    return [dict(it, score=s) for s, it in res[:limit]]


# ------------------------------------------------------------------ remote search

def search_openverse(query, kind="sfx", min_d=None, max_d=None, limit=10, license="safe"):
    params = {"q": query, "page_size": min(50, max(limit * 3, 20)), "mature": "false"}
    if kind == "music":
        params["source"] = "jamendo,ccmixter,wikimedia_audio"
    else:
        params["source"] = "freesound,wikimedia_audio"
    if license == "safe":
        params["license_type"] = "commercial,modification"
    elif license == "commercial":
        params["license_type"] = "commercial"
    if max_d and max_d <= 30:
        params["length"] = "shortest"
    url = "https://api.openverse.org/v1/audio/?" + urllib.parse.urlencode(params)
    data = _get_json(url)
    out = []
    for r in data.get("results", []):
        d = (r.get("duration") or 0) / 1000.0 or None
        if d is not None and ((min_d and d < min_d) or (max_d and d > max_d)):
            continue
        lic = _lic_norm(r.get("license"))
        out.append({
            "id": "ov:" + r["id"], "title": r.get("title") or "", "duration": round(d, 2) if d else None,
            "source": r.get("source"), "license": lic, "license_name": LICENSES.get(lic, lic),
            "author": r.get("creator"), "tags": [t.get("name") for t in (r.get("tags") or [])][:10],
            "url": r.get("url"), "page": r.get("foreign_landing_url"), "attribution": r.get("attribution"),
            "kind": kind, "genres": r.get("genres") or None,
        })
        if len(out) >= limit:
            break
    return out


def freesound_token():
    return os.environ.get("FREESOUND_TOKEN", "").strip() or os.environ.get("FREESOUND_API_KEY", "").strip()


def search_freesound(query, kind="sfx", min_d=None, max_d=None, limit=10, license="safe", sort="score"):
    tok = freesound_token()
    if not tok:
        return []
    flt = []
    if min_d or max_d:
        flt.append(f"duration:[{min_d or 0} TO {max_d or '*'}]")
    if license in ("safe", "commercial"):
        flt.append('license:("Creative Commons 0" OR "Attribution")')
    params = {"query": query, "page_size": min(50, limit), "sort": sort, "token": tok,
              "fields": "id,name,duration,license,username,tags,previews,avg_rating,num_ratings,num_downloads,url,description"}
    if flt:
        params["filter"] = " ".join(flt)
    data = _get_json("https://freesound.org/apiv2/search/text/?" + urllib.parse.urlencode(params))
    out = []
    for r in data.get("results", []):
        lic = _lic_norm(r.get("license"))
        pv = r.get("previews") or {}
        out.append({
            "id": f"fs:{r['id']}", "title": r.get("name"), "duration": round(r.get("duration") or 0, 2),
            "source": "freesound", "license": lic, "license_name": LICENSES.get(lic, lic), "author": r.get("username"),
            "tags": (r.get("tags") or [])[:10], "url": pv.get("preview-hq-mp3") or pv.get("preview-lq-mp3"),
            "page": r.get("url"), "rating": round(r.get("avg_rating") or 0, 1), "downloads": r.get("num_downloads"),
            "attribution": f'"{r.get("name")}" by {r.get("username")} (freesound.org) - {LICENSES.get(lic, lic)}',
            "kind": kind, "description": (r.get("description") or "")[:160],
        })
    return out


_cache = {}


def search(query, kind="sfx", source="all", min_d=None, max_d=None, limit=8, license="safe"):
    if not query:
        raise ToolError("`query` is required (in English works best: 'whoosh fast', 'cinematic impact', 'crowd cheer')")
    res, errors = [], []
    srcs = ["local", "freesound", "openverse"] if source in (None, "", "all") else [s.strip() for s in source.split(",")]
    for s in srcs:
        try:
            if s == "local":
                res += search_local(query, kind, min_d, max_d, limit)
            elif s == "freesound":
                res += search_freesound(query, kind, min_d, max_d, limit, license)
            elif s == "openverse":
                if "freesound" in srcs and freesound_token() and kind != "music" and len(res) >= limit:
                    continue
                res += search_openverse(query, kind, min_d, max_d, limit, license)
            else:
                errors.append(f"unknown source {s}")
        except ToolError as e:
            errors.append(str(e))
        except Exception as e:
            errors.append(f"{s}: {e}")
    seen, uniq = set(), []
    for r in res:
        key = (r.get("title", "").lower(), r.get("author"))
        if key in seen:
            continue
        seen.add(key)
        uniq.append(r)
        _cache[r["id"]] = r
    return uniq, errors


# ------------------------------------------------------------------ download

def get(item_id, kind=None):
    """Download (or resolve) an item into the library -> index record with `path`."""
    idx = load_index()
    if item_id in idx["items"] and os.path.exists(idx["items"][item_id].get("path", "")):
        return idx["items"][item_id]
    if os.path.isfile(item_id):
        return {"id": _local_id(item_id), "path": os.path.abspath(item_id), "title": os.path.basename(item_id), "source": "local", "license": "local"}
    meta = _cache.get(item_id)
    if meta is None:
        if item_id.startswith("ov:"):
            r = _get_json(f"https://api.openverse.org/v1/audio/{item_id[3:]}/")
            lic = _lic_norm(r.get("license"))
            meta = {"id": item_id, "title": r.get("title"), "duration": (r.get("duration") or 0) / 1000, "source": r.get("source"),
                    "license": lic, "license_name": LICENSES.get(lic, lic), "author": r.get("creator"), "url": r.get("url"),
                    "page": r.get("foreign_landing_url"), "attribution": r.get("attribution"), "kind": kind or "sfx"}
        elif item_id.startswith("fs:"):
            tok = freesound_token()
            if not tok:
                raise ToolError("fs: ids need FREESOUND_TOKEN (plugins/montage/plugin.toml [env])")
            r = _get_json(f"https://freesound.org/apiv2/sounds/{item_id[3:]}/?token={tok}")
            lic = _lic_norm(r.get("license"))
            meta = {"id": item_id, "title": r.get("name"), "duration": r.get("duration"), "source": "freesound", "license": lic,
                    "license_name": LICENSES.get(lic, lic), "author": r.get("username"), "url": r["previews"]["preview-hq-mp3"],
                    "page": r.get("url"), "attribution": f'"{r.get("name")}" by {r.get("username")} (freesound.org) - {LICENSES.get(lic, lic)}',
                    "kind": kind or "sfx"}
        else:
            raise ToolError(f"unknown id `{item_id}` (use ids from `sfx`/`music` results, or a file path)")
    url = meta.get("url")
    if not url:
        raise ToolError(f"{item_id} has no downloadable file")
    k = meta.get("kind") or kind or "sfx"
    ext = os.path.splitext(urllib.parse.urlparse(url).path)[1].lower()
    if ext not in AUDIO_EXT:
        ext = ".mp3"
    folder = os.path.join(MEDIA, k, _slug(meta.get("source") or "web", 20))
    os.makedirs(folder, exist_ok=True)
    path = os.path.join(folder, f"{_slug(meta.get('title'))}_{item_id.split(':', 1)[1][:8]}{ext}")
    if not os.path.exists(path):
        req = urllib.request.Request(url, headers=UA)
        with urllib.request.urlopen(req, timeout=120) as r, open(path + ".part", "wb") as f:
            while True:
                b = r.read(1 << 16)
                if not b:
                    break
                f.write(b)
        os.replace(path + ".part", path)
    rec = {k2: v for k2, v in meta.items() if k2 not in ("score",)}
    rec.update(path=path, kind=k, downloaded=time.strftime("%Y-%m-%d"))
    rec["tags"] = [t for t in (meta.get("tags") or []) if t] or _tok(meta.get("title", ""))
    with _lock:
        idx = load_index()
        idx["items"][item_id] = rec
        save_index(idx)
    return rec


def update_record(item_id, **kw):
    with _lock:
        idx = load_index()
        if item_id in idx["items"]:
            idx["items"][item_id].update(kw)
            save_index(idx)


def record_for_path(path):
    ap = os.path.abspath(path).lower()
    for it in load_index()["items"].values():
        if os.path.abspath(it.get("path", "")).lower() == ap:
            return it
    return None
