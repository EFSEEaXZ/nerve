"""Direct test of the montage plugin modules (no MCP). Usage: python tests/test_montage.py [--quick]"""
import json, os, subprocess, sys, time, tempfile, traceback

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.path.join(tempfile.gettempdir(), "nerve-montage-test")
os.makedirs(WORK, exist_ok=True)
os.environ["NERVE_DATA"] = os.path.join(WORK, "data")
os.environ["NERVE_OUT"] = os.path.join(WORK, "out")
sys.path[:0] = [os.path.join(ROOT, "plugins", "_sdk"), os.path.join(ROOT, "plugins", "montage")]
sys.stdout = sys.__stdout__

import analysis as A, engine as E, sources as S  # noqa

quick = "--quick" in sys.argv
ok = fail = 0


def check(name, fn):
    global ok, fail
    t = time.time()
    try:
        r = fn()
        ok += 1
        print(f"  PASS {name} ({time.time() - t:.1f}s) {str(r)[:300]}", flush=True)
        return r
    except Exception as e:
        fail += 1
        print(f"  FAIL {name}: {e}\n{traceback.format_exc()[-1500:]}", flush=True)


def _raises(fn):
    try:
        fn()
        return False
    except Exception:
        return True


def sh(argv):
    subprocess.run(argv, check=True, capture_output=True, stdin=subprocess.DEVNULL)


FF = A.which("ffmpeg")
speech = os.path.join(WORK, "speech.wav")
if not os.path.exists(speech):
    ps = ("Add-Type -AssemblyName System.Speech; $s=New-Object System.Speech.Synthesis.SpeechSynthesizer; "
          f"$s.SetOutputToWaveFile('{speech}'); $s.Speak('Hello everyone. Today we build a great video.'); "
          "Start-Sleep -Milliseconds 10; $s.Speak('After a short pause, the second sentence begins. Sound design matters a lot.'); $s.Dispose()")
    subprocess.run(["powershell", "-NoProfile", "-Command", ps], check=True, stdin=subprocess.DEVNULL)
    # insert 1.2s of silence between sentences to test silence cutting
    sh([FF, "-y", "-v", "error", "-i", speech, "-af", "apad=pad_dur=0.5", os.path.join(WORK, "s1.wav")])
clip1 = os.path.join(WORK, "clip1.mp4")
clip2 = os.path.join(WORK, "clip2.mp4")
img = os.path.join(WORK, "pic.png")
if not os.path.exists(clip1):
    sh([FF, "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=30", "-i", speech, "-shortest", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", clip1])
    sh([FF, "-y", "-v", "error", "-f", "lavfi", "-i", "mandelbrot=s=1280x720:r=30", "-t", "5", "-c:v", "libx264", "-pix_fmt", "yuv420p", clip2])
    sh([FF, "-y", "-v", "error", "-f", "lavfi", "-i", "smptehdbars=s=1080x1350", "-frames:v", "1", img])

print("== analysis")
info = check("probe", lambda: A.probe(clip1))
check("contact sheet", lambda: A.contact_sheet(clip1, 6)[0])
check("scenes", lambda: A.scenes(clip2)["cuts"][:5])
sil = check("silences", lambda: A.silences(clip1, -35, 0.3))
check("loudness", lambda: A.loudness(clip1)["lufs"])

print("== sources")
res = check("openverse sfx whoosh", lambda: S.search("whoosh", "sfx", "openverse", max_d=5, limit=5))
wh = imp = mus = None
if res and res[0]:
    rec = check("get whoosh", lambda: S.get(res[0][0]["id"]))
    wh = rec and rec["path"]
res2 = check("openverse sfx impact", lambda: S.search("cinematic impact", "sfx", "openverse", max_d=6, limit=5))
if res2 and res2[0]:
    rec = check("get impact", lambda: S.get(res2[0][0]["id"]))
    imp = rec and rec["path"]
if imp:
    check("inspect impact", lambda: {k: v for k, v in A.inspect_audio(imp).items() if k in ("duration", "peak_time", "shape", "tone", "lufs")})
res3 = check("openverse music", lambda: S.search("upbeat electronic", "music", "openverse", min_d=30, limit=5))
if res3 and res3[0]:
    rec = check("get music", lambda: S.get(res3[0][0]["id"], "music"))
    mus = rec and rec["path"]
if mus:
    b = check("beats", lambda: (lambda r: {"tempo": r["tempo_bpm"], "n": len(r["beats"]), "down": r["downbeats"][:4], "hits": r["hits"], "sections": len(r["sections"])})(A.beats(mus, None, 60)))
check("local search", lambda: [x["title"] for x in S.search_local("whoosh")])

tr = None
if not quick:
    print("== transcribe")
    tr = check("transcribe (base)", lambda: (lambda r: (r["language"], r["device"], len(r["words"]), r["segments"][0]["text"], r["files"]["json"]))(A.transcribe(clip1, "en", "base")))

print("== timeline")
keep = (sil or {}).get("keep") or [[0, 3]]
video = [{"src": clip1, "in": k[0], "out": k[1]} for k in keep[:2]]
video += [{"src": clip2, "in": 0, "out": 3, "transition": {"type": "smoothleft", "duration": 0.4}, "zoom": {"from": 1, "to": 1.2}},
          {"image": img, "duration": 2, "transition": "fadeblack"}, {"color": "#101018", "duration": 1.0, "transition": "fade"}]
audio = []
if mus:
    audio.append({"src": mus, "role": "music", "at": 0, "fade_out": 2})
if wh:
    audio.append({"src": wh, "role": "sfx", "at": "cut:2-0.2"})
if imp:
    audio.append({"src": imp, "role": "sfx", "at": "cut:3", "align": "peak"})
    audio.append({"src": imp, "role": "sfx", "at": "cut:2", "align": "end", "volume_db": -6})
tl = {"size": "1080p", "fps": 30, "grade": "cinematic", "video": video, "audio": audio,
      "overlays": [{"text": "NERVE MONTAGE", "at": 0.2, "duration": 1.8, "pos": "lower-third", "box": True},
                   {"image": img, "at": "clip:2+0.5", "duration": 1.5, "w": 0.25, "x": 0.95, "y": 0.1}],
      "output": os.path.join(WORK, "out", "test16x9.mp4")}
if tr:
    tl["captions"] = {"src": tr[4], "style": "clean"}
pl = check("plan", lambda: E.summary(E.plan(tl)))
if pl:
    print(json.dumps(pl, indent=0)[:1500])
r1 = check("render preview", lambda: (lambda r: (r["output"], r["resolution"], r["render_seconds"], r["loudness"]))(E.render(tl, preview=True)))
r2 = check("render final", lambda: (lambda r: (r["output"], r["resolution"], r["render_seconds"], r["loudness"], r.get("credits_file"), r["duration"]))(E.render(tl)))
if r2:
    check("final duration matches", lambda: (abs(A.probe(r2[0])["duration"] - r2[5]) < 0.15) or (_ for _ in ()).throw(Exception(f"{A.probe(r2[0])['duration']} vs {r2[5]}")))
    check("final loudness within 0.6 LU", lambda: (abs(r2[3]["lufs"] - r2[3]["target"]) <= 0.6) or (_ for _ in ()).throw(Exception(str(r2[3]))))
    check("align end placement", lambda: [(a["at"], a.get("duration")) for a in E.plan(tl)["audio"] if a.get("align") == "end"])
    check("final sheet", lambda: A.contact_sheet(r2[0], 8)[0])
vt = dict(tl, size="vertical", output=os.path.join(WORK, "out", "test9x16.mp4"))
if tr:
    vt["captions"] = {"src": tr[4], "style": "pop"}
r3 = check("render vertical (blur fit, pop captions)", lambda: (lambda r: (r["output"], r["resolution"], r["render_seconds"], r["captions"]))(E.render(vt, preview=False)))
if r3:
    check("vertical sheet", lambda: A.contact_sheet(r3[0], 8)[0])
check("export otio", lambda: E.export_otio(tl, output=os.path.join(WORK, "out", "test.otio")))
check("bad timeline error", lambda: (_ for _ in ()).throw(Exception("no error")) if not _raises(lambda: E.plan({"video": [{"src": "nope.mp4"}]})) else "ok")


print(f"\n{ok} passed, {fail} failed")

