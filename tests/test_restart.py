#!/usr/bin/env python3
"""Check that background jobs survive a nerve restart.
usage: python tests/test_restart.py start            -> prints job id
       (restart nerve)
       python tests/test_restart.py check <job id>   -> waits for the job, expects 'survived'
"""
import sys

import test_smoke as s

s.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "restart-test", "version": "1"}}, stream=False)
if sys.argv[1] == "start":
    code = "import time\nfor i in range(20):\n    print('tick', i, flush=True)\n    time.sleep(1)\nprint('survived', flush=True)"
    res, text, _ = s.call("exec", {"code": code, "lang": "python", "background": True, "label": "restart-test"})
    print(text.split()[2])
else:
    res, text, _ = s.call("job", {"action": "wait", "id": sys.argv[2], "timeout_ms": 45000})
    print(text)
    ok = "survived" in text and "exited(0)" in text
    print("PASS job survived restart" if ok else "FAIL job did not survive")
    sys.exit(0 if ok else 1)
