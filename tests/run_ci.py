"""Built-in regression suites against an isolated server. No production state or credentials."""
from pathlib import Path
import os, subprocess, sys, tempfile, time, urllib.request
ROOT=Path(__file__).resolve().parents[1]
EXE=Path(sys.argv[1]).resolve() if len(sys.argv)>1 else ROOT/'target/release/nerve.exe'
with tempfile.TemporaryDirectory(prefix='nerve-ci-') as home:
    p=Path(home)
    (p/'config.toml').write_text('[server]\nlocal_addr="127.0.0.1:8510"\npublic_addr=""\n[ui]\ntray=false\n[updates]\nautomatic=false\n[plugins]\nenabled=false\n[tunnel]\nprovider="none"\n')
    with (p/'server.log').open('w',encoding='utf-8') as log:
        server=subprocess.Popen([str(EXE),'--home',home,'serve'],stdin=subprocess.DEVNULL,stdout=log,stderr=log)
        try:
            for _ in range(150):
                try:
                    with urllib.request.urlopen('http://127.0.0.1:8510/api/state',timeout=1):break
                except Exception:time.sleep(.2)
            else:raise RuntimeError('isolated server failed to start')
            for name in ['test_completion.py','test_smoke.py']:
                subprocess.run([sys.executable,str(ROOT/'tests'/name),'http://127.0.0.1:8510'],cwd=ROOT,check=True,timeout=180)
        finally:
            if os.name=='nt':subprocess.run(['taskkill','/PID',str(server.pid),'/T','/F'],capture_output=True)
            else:server.terminate()
            server.wait(timeout=10)
