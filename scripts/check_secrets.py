"""High-confidence credential check for tracked publishable files. Never prints matched values."""
from pathlib import Path
import re, subprocess, sys
ROOT=Path(__file__).resolve().parents[1]
files=subprocess.run(['git','ls-files','-z'],cwd=ROOT,check=True,capture_output=True).stdout.decode().split('\0')
patterns=[('GitHub token',re.compile(r'gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}')),('AWS access key',re.compile(r'AKIA[0-9A-Z]{16}')),('Private key',re.compile(r'-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----'))]
forbidden={'auth.json','control.key','.env','config.toml'}
issues=[]
for name in filter(None,files):
    p=ROOT/name
    if p.name in forbidden or name.startswith(('qa/','target/','browser/','nerve-data/')):issues.append((name,'private runtime file'))
    if p.suffix.lower() in ['.png','.jpg','.woff2','.ttf','.ico'] or not p.is_file():continue
    text=p.read_text(encoding='utf-8',errors='replace')
    for label,pattern in patterns:
        if pattern.search(text):issues.append((name,label))
if issues:
    for name,label in issues:print('FAIL',name,label)
    sys.exit(1)
print('Credential check passed:',sum(bool(n) for n in files),'tracked files')
