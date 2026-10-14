"""nerve plugin: a skill library that AIs read before a task and extend after it.

Skills use the open Agent Skills format (https://agentskills.io): a folder with SKILL.md
(YAML frontmatter `name` + `description`, markdown body) plus optional scripts/, references/, assets/.
The same folders work in Claude Code, Cursor, Codex and other skill-aware agents.
"""

import datetime as _dt
import hashlib
import io
import json
import os
import re
import shutil
import threading
import time
import urllib.request
import zipfile

from nerve_plugin import Plugin, ToolError

HERE = os.path.dirname(os.path.abspath(__file__))
LIBRARY = os.path.join(HERE, "library")
DATA = os.environ.get("NERVE_DATA") or os.path.join(os.path.expanduser("~"), ".nerve")
ROOT = os.environ.get("NERVE_SKILLS_DIR", "").strip() or os.path.join(DATA, "skills")
_extra = os.environ.get("NERVE_SKILLS_EXTRA")
if _extra is None:
    EXTRA = [os.path.join(os.path.expanduser("~"), ".agents", "skills"), os.path.join(os.path.expanduser("~"), ".claude", "skills")]
else:
    EXTRA = [x.strip() for x in _extra.split(os.pathsep if os.pathsep in _extra else ";") if x.strip()]
STATS = os.path.join(ROOT, ".stats.json")
SEEDED = os.path.join(ROOT, ".seeded.json")
NAME_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
LESSONS = "## Lessons learned"
_lock = threading.RLock()

p = Plugin("skills", version="1.0.0")


# ------------------------------------------------------------------ helpers

def _today():
    return _dt.date.today().isoformat()


def _now():
    return _dt.datetime.now().strftime("%Y%m%d-%H%M%S")


def _read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def _write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, path)


def _sha(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def _jload(path, default):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return default


def _jsave(path, obj):
    _write(path, json.dumps(obj, ensure_ascii=False, indent=1))


def norm_name(name):
    n = re.sub(r"[^a-z0-9]+", "-", str(name or "").strip().lower()).strip("-")
    n = re.sub(r"-{2,}", "-", n)[:64].strip("-")
    if not n:
        raise ToolError("skill name must be latin letters/digits/hyphens, e.g. `weapon-3d-blender`")
    return n


def _unq(v):
    v = v.strip()
    if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
        v = v[1:-1]
        if v and '"' in v:
            v = v.replace('\\"', '"')
    return v


def parse_skill_md(text):
    """-> (meta dict, body). Tolerant mini-YAML (PyYAML when available)."""
    text = text.lstrip("\ufeff")
    m = re.match(r"^---\s*\n(.*?)\n---\s*(?:\n|$)", text, re.S)
    if not m:
        return {}, text
    raw, body = m.group(1), text[m.end():]
    try:
        import yaml  # type: ignore
        meta = yaml.safe_load(raw) or {}
        if isinstance(meta, dict):
            return meta, body
    except Exception:
        pass
    meta, cur, block = {}, None, None
    for line in raw.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line[:1] in " \t" and cur:
            s = line.strip()
            if block is not None:
                meta[cur] = (meta[cur] + " " + s).strip()
            elif ":" in s:
                k, v = s.split(":", 1)
                if not isinstance(meta.get(cur), dict):
                    meta[cur] = {}
                meta[cur][k.strip()] = _unq(v)
            continue
        if ":" in line:
            k, v = line.split(":", 1)
            cur, v = k.strip(), v.strip()
            block = None
            if v in (">", "|", ">-", "|-"):
                meta[cur], block = "", True
            else:
                meta[cur] = _unq(v) if v else {}
    return meta, body


def _yq(s):
    s = str(s).replace("\n", " ").strip()
    return json.dumps(s, ensure_ascii=False)


def build_skill_md(meta, body):
    lines = ["---", f"name: {meta['name']}", f"description: {_yq(meta['description'])}"]
    for k, v in meta.items():
        if k in ("name", "description"):
            continue
        if isinstance(v, dict):
            lines.append(f"{k}:")
            lines += [f"  {kk}: {_yq(vv)}" for kk, vv in v.items()]
        elif v not in (None, ""):
            lines.append(f"{k}: {_yq(v)}")
    lines.append("---")
    return "\n".join(lines) + "\n\n" + body.strip("\n") + "\n"


def _validate(meta):
    name, desc = str(meta.get("name") or ""), str(meta.get("description") or "").strip()
    if not NAME_RE.match(name) or len(name) > 64:
        raise ToolError(f"invalid skill name `{name}` (lowercase latin, digits, hyphens; <= 64 chars)")
    if not desc:
        raise ToolError("description is required: say WHAT the skill does and WHEN to use it (keywords help `find`)")
    if len(desc) > 1024:
        raise ToolError(f"description too long ({len(desc)} > 1024 chars); move details into the body")


# ------------------------------------------------------------------ library

class Skill:
    def __init__(self, path, readonly=False):
        self.dir = path
        self.file = os.path.join(path, "SKILL.md")
        self.readonly = readonly
        self.text = _read(self.file)
        self.meta, self.body = parse_skill_md(self.text)
        self.name = str(self.meta.get("name") or os.path.basename(path))
        self.description = str(self.meta.get("description") or "").strip()

    def files(self):
        out = []
        for root, dirs, files in os.walk(self.dir):
            dirs[:] = [d for d in dirs if not d.startswith(".") and d != "__pycache__"]
            for f in files:
                if f.startswith(".") or f == "SKILL.md":
                    continue
                fp = os.path.join(root, f)
                out.append((os.path.relpath(fp, self.dir).replace("\\", "/"), os.path.getsize(fp)))
        return sorted(out)


def _roots():
    yield ROOT, False
    for r in EXTRA:
        if os.path.isdir(r) and os.path.abspath(r) != os.path.abspath(ROOT):
            yield r, True


def all_skills():
    seen, out = set(), []
    for root, ro in _roots():
        try:
            names = sorted(os.listdir(root))
        except OSError:
            continue
        for d in names:
            path = os.path.join(root, d)
            if d.startswith(".") or not os.path.isfile(os.path.join(path, "SKILL.md")):
                continue
            try:
                s = Skill(path, ro)
            except Exception:
                continue
            if s.name in seen:
                continue
            seen.add(s.name)
            out.append(s)
    return out


def get_skill(name, writable=False):
    want = str(name or "").strip()
    if not want:
        raise ToolError("`name` is required")
    cands = all_skills()
    for s in cands:
        if s.name == want or os.path.basename(s.dir) == want:
            break
    else:
        try:
            n = norm_name(want)
        except ToolError:
            n = want
        s = next((s for s in cands if s.name == n), None)
        if s is None:
            close = [x.name for x in search(want, cands)[:5]]
            raise ToolError(f"no skill `{want}`" + (f"; closest: {', '.join(close)}" if close else "") + ". Use `list` or `find`.")
    if writable and s.readonly:
        raise ToolError(f"`{s.name}` lives in a read-only root ({os.path.dirname(s.dir)}); copy it with `save` first")
    return s


def _backup(s):
    hist = os.path.join(ROOT, ".history", s.name)
    os.makedirs(hist, exist_ok=True)
    shutil.copy2(s.file, os.path.join(hist, _now() + ".md"))
    old = sorted(os.listdir(hist))
    for f in old[:-20]:
        try:
            os.remove(os.path.join(hist, f))
        except OSError:
            pass


def _stats():
    return _jload(STATS, {})


def _bump(name, **inc):
    with _lock:
        st = _stats()
        e = st.setdefault(name, {})
        for k, v in inc.items():
            if isinstance(v, int) and not isinstance(v, bool):
                e[k] = e.get(k, 0) + v
            else:
                e[k] = v
        _jsave(STATS, st)


# ------------------------------------------------------------------ search

def _tokens(text):
    out = []
    for t in re.findall(r"\w+", str(text).lower()):
        if len(t) < 2:
            continue
        out.append(t[:6] if len(t) > 6 else t)  # crude stemming, works for ru/en
    return out


def search(query, skills=None):
    skills = all_skills() if skills is None else skills
    q = set(_tokens(query))
    if not q:
        return skills
    st = _stats()
    scored = []
    for s in skills:
        name_t, desc_t, body_t = _tokens(s.name.replace("-", " ")), _tokens(s.description), _tokens(s.body)
        score = 0.0
        for t in q:
            score += 4 * sum(1 for x in name_t if x.startswith(t) or t.startswith(x))
            score += 2 * min(desc_t.count(t), 3)
            score += min(body_t.count(t), 4) * 0.5
        if score <= 0:
            continue
        e = st.get(s.name, {})
        ok, bad = e.get("success", 0), e.get("fail", 0)
        score *= 1 + 0.1 * (ok - bad) / (ok + bad + 2)
        scored.append((score, s))
    scored.sort(key=lambda x: -x[0])
    return [s for _, s in scored]


# ------------------------------------------------------------------ seeding bundled skills

def seed():
    if not os.path.isdir(LIBRARY):
        return
    os.makedirs(ROOT, exist_ok=True)
    rec = _jload(SEEDED, {})
    changed = False
    for root, dirs, files in os.walk(LIBRARY):
        dirs[:] = [d for d in dirs if d != "__pycache__"]
        for f in files:
            src = os.path.join(root, f)
            rel = os.path.relpath(src, LIBRARY).replace("\\", "/")
            dst = os.path.join(ROOT, rel)
            new = _read(src)
            h_new = _sha(new)
            if rel in rec:
                if not os.path.exists(dst):
                    continue  # deleted by the user/AI: respect it
                cur = _read(dst)
                if _sha(cur) == h_new or _sha(cur) != rec[rel]:
                    continue  # up to date, or edited (learned) -> keep
            elif os.path.exists(dst):
                rec[rel] = _sha(_read(dst))
                changed = True
                continue
            _write(dst, new)
            rec[rel] = h_new
            changed = True
    if changed:
        _jsave(SEEDED, rec)


# ------------------------------------------------------------------ actions

def _usage(name):
    e = _stats().get(name, {})
    bits = []
    if e.get("uses"):
        bits.append(f"used {e['uses']}x")
    if e.get("success") or e.get("fail"):
        bits.append(f"ok {e.get('success', 0)} / fail {e.get('fail', 0)}")
    return f" [{', '.join(bits)}]" if bits else ""


def act_list(a, ctx):
    skills = search(a["query"]) if a.get("query") else all_skills()
    limit = int(a.get("limit") or 200)
    if not skills:
        return f"no skills yet in {ROOT}. Create one with `record` or `save`, or `install` from GitHub."
    lines = [f"{len(skills)} skill(s) in {ROOT}" + (" (+ read-only roots)" if any(s.readonly for s in skills) else "")]
    for s in skills[:limit]:
        d = s.description if len(s.description) <= 220 else s.description[:217] + "..."
        lines.append(f"- {s.name}{' (read-only)' if s.readonly else ''}{_usage(s.name)}: {d}")
    if len(skills) > limit:
        lines.append(f"... {len(skills) - limit} more (use `find`)")
    lines.append("Next: `read {name}` the relevant one(s) before you start.")
    return "\n".join(lines)


def act_find(a, ctx):
    q = a.get("query") or a.get("name")
    if not q:
        raise ToolError("`query` is required (describe the task)")
    res = search(q)[: int(a.get("limit") or 5)]
    if not res:
        return f"no skill matches `{q}`. Do the task, then `record` how you did it so the next AI starts ahead."
    out = [f"top {len(res)} for `{q}`:"]
    for s in res:
        out.append(f"- {s.name}{_usage(s.name)}: {s.description}")
    out.append(f"Next: skills read {{name: \"{res[0].name}\"}} and follow it.")
    return "\n".join(out)


def _safe_join(base, rel):
    path = os.path.abspath(os.path.join(base, rel))
    if os.path.commonpath([path, os.path.abspath(base)]) != os.path.abspath(base):
        raise ToolError(f"path `{rel}` escapes the skill folder")
    return path


def act_read(a, ctx):
    s = get_skill(a.get("name"))
    f = a.get("file")
    if f:
        path = _safe_join(s.dir, f)
        if not os.path.isfile(path):
            raise ToolError(f"`{f}` not found; files: {', '.join(x for x, _ in s.files()) or 'none'}")
        return f"# {s.name}/{f}\n\n" + _read(path)
    _bump(s.name, uses=1, last_used=_today())
    files = s.files()
    out = [f"# skill `{s.name}` ({s.dir})", s.text.rstrip()]
    if files:
        out.append("\n---\nFiles (read with `read {name, file}` when the instructions say so):")
        out += [f"- {r} ({n} B)" for r, n in files]
    out.append("\n---\nWhen done: `learn {name, lesson}` for anything you'd tell the next AI, `rate {name, outcome}`.")
    return "\n".join(out)


def _write_files(dirpath, files):
    if not files:
        return []
    if not isinstance(files, dict):
        raise ToolError("`files` must be an object {\"references/x.md\": \"text\", ...}")
    done = []
    for rel, content in files.items():
        if os.path.basename(rel).upper() == "SKILL.MD":
            raise ToolError("put SKILL.md content in `body`, not `files`")
        path = _safe_join(dirpath, rel)
        _write(path, content if isinstance(content, str) else json.dumps(content, ensure_ascii=False, indent=1))
        done.append(rel)
    return done


def act_save(a, ctx):
    body = a.get("body") or ""
    meta, b = parse_skill_md(body) if body.lstrip().startswith("---") else ({}, body)
    name = norm_name(a.get("name") or meta.get("name"))
    meta["name"] = name
    if a.get("description"):
        meta["description"] = a["description"]
    if isinstance(a.get("metadata"), dict):
        meta.setdefault("metadata", {}).update({k: str(v) for k, v in a["metadata"].items()})
    with _lock:
        dirpath = os.path.join(ROOT, name)
        exists = os.path.isfile(os.path.join(dirpath, "SKILL.md"))
        if exists and not a.get("overwrite"):
            raise ToolError(f"skill `{name}` exists. Use `learn`/`edit` to improve it, or pass overwrite:true to replace (old version is kept in .history).")
        if exists:
            old = Skill(dirpath)
            _backup(old)
            for k, v in old.meta.items():
                meta.setdefault(k, v)
        if not b.strip():
            raise ToolError("`body` is required (markdown instructions)")
        _validate(meta)
        _write(os.path.join(dirpath, "SKILL.md"), build_skill_md(meta, b))
        written = _write_files(dirpath, a.get("files"))
        _bump(name, updated=_today(), **({} if exists else {"created": _today()}))
    lines = len(b.splitlines())
    note = " (tip: keep SKILL.md under ~500 lines; move details to references/)" if lines > 500 else ""
    return f"{'replaced' if exists else 'created'} skill `{name}` ({lines} lines){note}" + (f"; files: {', '.join(written)}" if written else "")


def _bullets(items):
    if not items:
        return []
    if isinstance(items, str):
        items = [x for x in items.splitlines() if x.strip()]
    return [re.sub(r"^\s*(?:[-*]|\d+[.)])\s*", "", str(x)).strip() for x in items if str(x).strip()]


def _append_lesson(s, entry):
    text = s.text.rstrip("\n")
    if LESSONS in text:
        i = text.index(LESSONS) + len(LESSONS)
        nxt = re.search(r"\n#{1,2} ", text[i:])
        if nxt:
            j = i + nxt.start()
            text = text[:j].rstrip("\n") + "\n" + entry + "\n" + text[j:]
        else:
            text = text + "\n" + entry
    else:
        text = text + f"\n\n{LESSONS}\n\n" + entry
    _backup(s)
    _write(s.file, text + "\n")
    sec = text[text.index(LESSONS):]
    return sum(1 for l in sec.splitlines() if l.startswith("- ["))


def act_learn(a, ctx):
    lesson = (a.get("lesson") or a.get("note") or "").strip()
    if not lesson:
        raise ToolError("`lesson` is required: one concrete, reusable sentence (what to do / avoid, with numbers or commands)")
    with _lock:
        s = get_skill(a.get("name"), writable=True)
        kind = (a.get("kind") or "tip").strip()
        entry = f"- [{_today()}] ({kind}) " + lesson.replace("\n", " ")
        if a.get("task"):
            entry += f" (task: {a['task']})"
        n = _append_lesson(s, entry)
        _bump(s.name, updated=_today())
    msg = f"added to `{s.name}` -> {LESSONS} ({n} entries)"
    if n >= 25:
        msg += ". The list is long: fold the durable lessons into the main steps with `edit`/`save overwrite` and prune the rest."
    return msg


def act_record(a, ctx):
    task = (a.get("task") or "").strip()
    steps = _bullets(a.get("steps"))
    if not task or not steps:
        raise ToolError("`task` and `steps` are required: what you achieved and the exact steps that worked (tools, params, commands)")
    pitfalls, checks = _bullets(a.get("pitfalls")), _bullets(a.get("checks"))
    result, tools = (a.get("result") or "").strip(), _bullets(a.get("tools"))
    name = norm_name(a.get("name") or task)
    with _lock:
        try:
            s = get_skill(name, writable=True)
        except ToolError as e:
            if "read-only" in str(e):
                raise
            s = None
        if s is not None:
            parts = [f"run: {task}"]
            if result:
                parts.append(f"result: {result}")
            if pitfalls:
                parts.append("pitfalls: " + "; ".join(pitfalls))
            if steps:
                parts.append("steps used: " + " -> ".join(steps))
            n = _append_lesson(s, f"- [{_today()}] (run) " + " | ".join(parts))
            _bump(s.name, updated=_today())
            return f"`{s.name}` already exists: appended this run to {LESSONS} ({n} entries). If the main steps are now outdated, fix them with `edit`."
        desc = (a.get("description") or "").strip()
        if not desc:
            raise ToolError("new skill: `description` is required (what it does + when to use it, with trigger keywords)")
        title = a.get("title") or task[:1].upper() + task[1:]
        body = [f"# {title}", ""]
        if a.get("summary"):
            body += [a["summary"].strip(), ""]
        if tools:
            body += ["## Tools", ""] + [f"- {t}" for t in tools] + [""]
        body += ["## Steps", ""] + [f"{i}. {x}" for i, x in enumerate(steps, 1)] + [""]
        if pitfalls:
            body += ["## Pitfalls", ""] + [f"- {x}" for x in pitfalls] + [""]
        if checks:
            body += ["## Verify before you finish", ""] + [f"- [ ] {x}" for x in checks] + [""]
        body += [LESSONS, "", f"- [{_today()}] (recorded) {task}" + (f" -> {result}" if result else "")]
        meta = {"name": name, "description": desc, "metadata": {"author": a.get("author") or "nerve-ai", "created": _today()}}
        _validate(meta)
        dirpath = os.path.join(ROOT, name)
        _write(os.path.join(dirpath, "SKILL.md"), build_skill_md(meta, "\n".join(body)))
        written = _write_files(dirpath, a.get("files"))
        _bump(name, created=_today(), updated=_today())
    return f"recorded new skill `{name}` ({len(steps)} steps)" + (f"; files: {', '.join(written)}" if written else "") + ". Future AIs will find it via `find`."


def act_rate(a, ctx):
    out = (a.get("outcome") or "").lower()
    if out not in ("success", "fail", "ok", "failed", "good", "bad"):
        raise ToolError("`outcome` must be success | fail")
    good = out in ("success", "ok", "good")
    s = get_skill(a.get("name"))
    _bump(s.name, **{"success" if good else "fail": 1, "last_rated": _today()})
    msg = f"rated `{s.name}`: {'success' if good else 'fail'}{_usage(s.name)}"
    if a.get("note") and not s.readonly:
        _append_lesson(get_skill(s.name), f"- [{_today()}] ({'worked' if good else 'failed'}) {a['note']}")
        msg += "; note added to lessons"
    return msg


def act_edit(a, ctx):
    old, new = a.get("old"), a.get("new")
    if old is None or new is None:
        raise ToolError("`old` and `new` are required (exact text replacement)")
    with _lock:
        s = get_skill(a.get("name"), writable=True)
        path = _safe_join(s.dir, a.get("file") or "SKILL.md")
        text = _read(path)
        n = text.count(old)
        if n == 0:
            raise ToolError("`old` text not found (copy it exactly from `read`)")
        if n > 1 and not a.get("all"):
            raise ToolError(f"`old` matches {n} times; add context or pass all:true")
        if path == s.file:
            _backup(s)
        text = text.replace(old, new)
        if path == s.file:
            meta, _ = parse_skill_md(text)
            meta.setdefault("name", s.name)
            _validate(meta)
        _write(path, text)
        _bump(s.name, updated=_today())
    return f"edited `{s.name}`/{a.get('file') or 'SKILL.md'} ({n} replacement(s))"


def act_attach(a, ctx):
    f, content = a.get("file"), a.get("content")
    if not f or content is None:
        raise ToolError("`file` (e.g. references/ffmpeg.md, scripts/do.py) and `content` are required")
    with _lock:
        s = get_skill(a.get("name"), writable=True)
        _write_files(s.dir, {f: content})
        _bump(s.name, updated=_today())
    return f"wrote `{s.name}`/{f}. Mention in SKILL.md WHEN to read/run it."


def act_delete(a, ctx):
    with _lock:
        s = get_skill(a.get("name"), writable=True)
        trash = os.path.join(ROOT, ".trash", f"{s.name}-{_now()}")
        os.makedirs(os.path.dirname(trash), exist_ok=True)
        shutil.move(s.dir, trash)
    return f"moved `{s.name}` to {trash}"


def _gh_parse(src):
    m = re.match(r"https?://github\.com/([^/]+)/([^/#?]+)(?:/(?:tree|blob)/([^/]+)/?(.*))?", src)
    if not m:
        return None
    owner, repo, ref, sub = m.group(1), m.group(2).removesuffix(".git"), m.group(3), (m.group(4) or "").strip("/")
    if sub.endswith("SKILL.md"):
        sub = sub[: -len("SKILL.md")].strip("/")
    return owner, repo, ref, sub


def _install_dir(src_dir, overwrite, installed, skipped):
    s = Skill(src_dir)
    name = norm_name(s.name)
    dst = os.path.join(ROOT, name)
    if os.path.exists(dst):
        if not overwrite:
            skipped.append(name)
            return
        _backup(Skill(dst))
        shutil.rmtree(dst)
    shutil.copytree(src_dir, dst, ignore=shutil.ignore_patterns(".git", "__pycache__"))
    _bump(name, created=_today(), source="install")
    installed.append(name)


def act_install(a, ctx):
    src = (a.get("source") or a.get("url") or "").strip()
    if not src:
        raise ToolError("`source` is required: GitHub URL (repo or folder), local folder or .zip")
    overwrite = bool(a.get("overwrite"))
    installed, skipped = [], []
    tmp = os.path.join(ROOT, ".tmp", _now())
    try:
        if os.path.isdir(src):
            base = src
        else:
            os.makedirs(tmp, exist_ok=True)
            if os.path.isfile(src):
                data, sub = open(src, "rb").read(), ""
            else:
                gh = _gh_parse(src)
                if gh:
                    owner, repo, ref, sub = gh
                    urls = [f"https://codeload.github.com/{owner}/{repo}/zip/{ref}"] if ref else [
                        f"https://codeload.github.com/{owner}/{repo}/zip/refs/heads/main",
                        f"https://codeload.github.com/{owner}/{repo}/zip/refs/heads/master"]
                else:
                    urls, sub = [src], ""
                data, err = None, None
                for u in urls:
                    try:
                        ctx.progress(f"downloading {u}")
                        req = urllib.request.Request(u, headers={"User-Agent": "nerve-skills/1.0"})
                        data = urllib.request.urlopen(req, timeout=120).read()
                        break
                    except Exception as e:
                        err = e
                if data is None:
                    raise ToolError(f"download failed: {err}")
            zipfile.ZipFile(io.BytesIO(data)).extractall(tmp)
            tops = [d for d in os.listdir(tmp) if os.path.isdir(os.path.join(tmp, d))]
            base = os.path.join(tmp, tops[0]) if len(tops) == 1 and not os.path.isfile(os.path.join(tmp, "SKILL.md")) else tmp
            if sub:
                base = os.path.join(base, *sub.split("/"))
                if not os.path.isdir(base):
                    raise ToolError(f"folder `{sub}` not found in the archive")
        if os.path.isfile(os.path.join(base, "SKILL.md")):
            _install_dir(base, overwrite, installed, skipped)
        else:
            for root, dirs, files in os.walk(base):
                dirs[:] = [d for d in dirs if not d.startswith(".") and d not in ("node_modules", "__pycache__")]
                if "SKILL.md" in files:
                    _install_dir(root, overwrite, installed, skipped)
                    dirs[:] = []
        if not installed and not skipped:
            raise ToolError("no SKILL.md found in the source")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    msg = f"installed {len(installed)}: {', '.join(installed) or '-'}"
    if skipped:
        msg += f"; skipped existing {len(skipped)}: {', '.join(skipped)} (overwrite:true to replace)"
    return msg


def act_dir(a, ctx):
    return json.dumps({"root": ROOT, "read_only_roots": [r for r, ro in _roots() if ro], "bundled": LIBRARY}, indent=1)


def _card(s, st):
    e = st.get(s.name, {})
    return {
        "name": s.name, "description": s.description, "readonly": s.readonly, "dir": s.dir,
        "lines": len(s.body.splitlines()), "files": [{"path": r, "size": n} for r, n in s.files()],
        "uses": e.get("uses", 0), "success": e.get("success", 0), "fail": e.get("fail", 0),
        "created": e.get("created"), "updated": e.get("updated"), "last_used": e.get("last_used"),
    }


def act_index(a, ctx):
    """Machine-readable list for the dashboard (does not count as a use)."""
    st = _stats()
    return json.dumps({"root": ROOT, "skills": [_card(s, st) for s in all_skills()]}, ensure_ascii=False)


def act_source(a, ctx):
    """Raw SKILL.md (or one attached file) for the dashboard editor."""
    s = get_skill(a.get("name"))
    f = a.get("file")
    text = _read(_safe_join(s.dir, f)) if f else s.text
    return json.dumps({**_card(s, _stats()), "file": f or "SKILL.md", "text": text}, ensure_ascii=False)


ACTIONS = {
    "index": act_index, "source": act_source,
    "list": act_list, "find": act_find, "read": act_read, "save": act_save, "record": act_record,
    "learn": act_learn, "rate": act_rate, "edit": act_edit, "attach": act_attach, "delete": act_delete,
    "install": act_install, "dir": act_dir,
}

SCHEMA = {
    "type": "object",
    "properties": {
        "action": {"type": "string", "enum": sorted(ACTIONS), "description": "What to do"},
        "name": {"type": "string", "description": "Skill name (lowercase-hyphen, e.g. weapon-3d-blender)"},
        "query": {"type": "string", "description": "find/list: the task in your own words (ru/en)"},
        "file": {"type": "string", "description": "read/edit/attach: file inside the skill (references/x.md, scripts/y.py)"},
        "description": {"type": "string", "description": "save/record: WHAT it does + WHEN to use it (<=1024 chars, include trigger keywords)"},
        "body": {"type": "string", "description": "save: SKILL.md markdown body (or a full SKILL.md with frontmatter)"},
        "files": {"type": "object", "description": "save/record: extra files {relative path: text}"},
        "overwrite": {"type": "boolean", "description": "save/install: replace an existing skill (old version kept)"},
        "task": {"type": "string", "description": "record: what was achieved; learn: optional context"},
        "title": {"type": "string"},
        "summary": {"type": "string", "description": "record: 1-3 sentences on the approach"},
        "steps": {"type": "array", "items": {"type": "string"}, "description": "record: exact steps that worked (tools, params, commands)"},
        "pitfalls": {"type": "array", "items": {"type": "string"}, "description": "record: what went wrong and how to avoid it"},
        "checks": {"type": "array", "items": {"type": "string"}, "description": "record: how to verify the result"},
        "tools": {"type": "array", "items": {"type": "string"}, "description": "record: tools/apps/libraries used"},
        "result": {"type": "string", "description": "record: outcome / quality notes"},
        "lesson": {"type": "string", "description": "learn: one concrete reusable insight"},
        "kind": {"type": "string", "description": "learn: tip | pitfall | fix | taste (default tip)"},
        "outcome": {"type": "string", "enum": ["success", "fail"], "description": "rate"},
        "note": {"type": "string", "description": "rate: optional lesson to store with the rating"},
        "old": {"type": "string"}, "new": {"type": "string"},
        "all": {"type": "boolean"},
        "content": {"type": "string", "description": "attach: file content"},
        "source": {"type": "string", "description": "install: GitHub repo/folder URL (e.g. https://github.com/anthropics/skills/tree/main/skills/pptx), local folder or .zip"},
        "metadata": {"type": "object"},
        "limit": {"type": "integer"},
    },
    "required": ["action"],
}


@p.tool(
    schema=SCHEMA,
    description=(
        "Skill library: proven how-to instructions written by earlier AIs and the user (Agent Skills / SKILL.md format). "
        "BEFORE any non-trivial or creative task (video editing/montage, sound design, Blender, Unreal, documents, deploys, scraping...) "
        "call find {query} and read {name} the best match, then follow it - it encodes taste and pitfalls you would otherwise repeat. "
        "AFTER the task, pay it forward: learn {name, lesson} for a tip/pitfall, record {name, task, steps, pitfalls?, checks?, description} for a new procedure, rate {name, outcome}. "
        "Write for a future AI with zero context: concrete tools, parameters, numbers, file paths, what looked bad and why. "
        "Actions: list {query?}; find {query}; read {name, file?}; record; learn; rate; save {name, description, body, files?, overwrite?}; "
        "edit {name, old, new, file?}; attach {name, file, content}; install {source: GitHub URL|folder|zip}; delete {name}; dir."
    ),
    annotations={"title": "Skills", "destructiveHint": False, "openWorldHint": True},
)
def skills(ctx, **a):
    act = a.get("action") or "list"
    fn = ACTIONS.get(act)
    if fn is None:
        raise ToolError(f"unknown action `{act}`; valid: {', '.join(sorted(ACTIONS))}")
    return fn(a, ctx)


try:
    seed()
except Exception as e:  # never block startup
    print(f"skills: seeding failed: {e}", flush=True)

if __name__ == "__main__":
    p.run()
