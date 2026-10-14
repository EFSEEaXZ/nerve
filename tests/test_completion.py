"""Completion/transport regression tests. Run only against an isolated QA server."""
import concurrent.futures, json, statistics, sys, time, urllib.request
BASE = sys.argv[1].rstrip('/') if len(sys.argv)>1 else 'http://127.0.0.1:8510'
SID = None
N = 0
CHECKS = []
def rpc(method, params=None, accept='application/json, text/event-stream', rid=None):
    global N, SID
    N += 1
    rid = rid if rid is not None else N
    h={'Content-Type':'application/json','Accept':accept}
    if SID: h['Mcp-Session-Id']=SID
    body=json.dumps({'jsonrpc':'2.0','id':rid,'method':method,'params':params or {}}).encode()
    t=time.perf_counter()
    with urllib.request.urlopen(urllib.request.Request(BASE+'/mcp',data=body,headers=h),timeout=15) as r:
        SID=r.headers.get('Mcp-Session-Id') or SID
        ct=r.headers.get('Content-Type','')
        # read() must return at EOF, not just when a final event happens to appear.
        raw=r.read().decode()
    elapsed=(time.perf_counter()-t)*1000
    events=[json.loads(l[5:]) for l in raw.splitlines() if l.startswith('data:')] if 'event-stream' in ct else [json.loads(raw)]
    final=[v for v in events if v.get('id')==rid and ('result' in v or 'error' in v)]
    assert len(final)==1,(rid,ct,events)
    assert events[-1] is final[0],events
    return final[0],ct,elapsed,events

def check(name, condition, details=''):
    CHECKS.append((name,bool(condition),details))
    print(('PASS ' if condition else 'FAIL ')+name+' '+str(details),flush=True)
    assert condition,name

def call(args, accept='application/json, text/event-stream', rid=None):
    return rpc('tools/call',{'name':'exec','arguments':args},accept,rid)

rpc('initialize',{'protocolVersion':'2025-11-25','capabilities':{},'clientInfo':{'name':'completion-qa','version':'1'}})
samples=[]
for i in range(15):
    r,ct,ms,ev=rpc('tools/call',{'name':'fs','arguments':{'action':'stat','path':'.'}})
    check('quick result '+str(i),'application/json' in ct and 'result' in r,round(ms,1))
    samples.append(ms)
r,ct,ms,_=call({'lang':'python','code':"print('quick-success')"})
check('quick exec completes','quick-success' in json.dumps(r) and ms<2000,(ct,round(ms,1)))
r,ct,ms,ev=rpc('tools/call',{'name':'exec','arguments':{'lang':'python','code':"import time; print('start',flush=True); time.sleep(2.2); print('end',flush=True)"},'_meta':{'progressToken':'qa-progress'}})
check('slow exec retains SSE','text/event-stream' in ct,round(ms,1))
check('progress forwarded',any(v.get('method')=='notifications/progress' for v in ev),len(ev))
check('stream closes at completion',ms<4000 and 'end' in json.dumps(r),round(ms,1))
r,ct,ms,_=call({'lang':'python','code':"import time; time.sleep(.6); print('without-progress')"})
check('slow SSE without progress closes','event-stream' in ct and ms<2000,round(ms,1))
r,ct,ms,_=call({'lang':'python','code':"print('sse-only')"},'text/event-stream')
check('SSE-only client respected','event-stream' in ct,round(ms,1))
r,ct,ms,_=call({'lang':'python','code':"import time; time.sleep(.4); print('json-only')"},'application/json')
check('JSON-only client respected','application/json' in ct,round(ms,1))
r,ct,ms,_=call({'lang':'python','code':'import sys; sys.exit(7)'})
check('failure returns a final result',r['result']['isError'] is True)
r,ct,ms,_=rpc('tools/call',{'name':'missing-tool','arguments':{}})
check('unknown tool returns final RPC error',r.get('error',{}).get('code')==-32602)
r,ct,ms,_=call({'lang':'python','code':'import time; time.sleep(30)','timeout_ms':150})
check('timeout completes',r['result']['isError'] and ms<3000,round(ms,1))
# A completed parent can leave inherited stdout open in its child. It must not stall the agent.
r,ct,ms,_=call({'lang':'python','code':"import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time; time.sleep(4)']); print('parent-done',flush=True)"})
check('inherited output pipe is bounded','parent-done' in json.dumps(r) and ms<1400,round(ms,1))
r,ct,ms,_=call({'lang':'python','code':"print('X'*300000)",'max_output':0})
check('large output saved rather than stalled','large response saved in full' in json.dumps(r) and len(json.dumps(r))<52000 and ms<3000,(round(ms,1),len(json.dumps(r))))
# Cancellation uses an existing session; do not mutate SID while sending the notification.
with concurrent.futures.ThreadPoolExecutor() as pool:
    future=pool.submit(call,{'lang':'python','code':'import time; time.sleep(30)'},'application/json, text/event-stream',8801)
    time.sleep(.6)
    note={'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':8801}}
    h={'Content-Type':'application/json','Mcp-Session-Id':SID}
    with urllib.request.urlopen(urllib.request.Request(BASE+'/mcp',data=json.dumps(note).encode(),headers=h),timeout=5) as r: r.read()
    result,ct,ms,_=future.result(timeout=5)
check('cancelled call completes',result['result']['isError'] and ms<4000,round(ms,1))
# Background tools return promptly and expose a job ID rather than holding the request.
r,ct,ms,_=call({'lang':'python','code':"import time; time.sleep(.5); print('job-done')",'background':True})
check('background returns promptly',ms<2000 and 'job' in json.dumps(r),round(ms,1))
with urllib.request.urlopen(BASE+'/api/state',timeout=5) as r: state=json.load(r)
check('inflight cleared',all(s['inflight']==0 for s in state['sessions']))
summary={'checks':len(CHECKS),'passed':sum(x[1] for x in CHECKS),'fast_median_ms':round(statistics.median(samples),2),'fast_max_ms':round(max(samples),2),'details':CHECKS}
print(json.dumps(summary,ensure_ascii=False),flush=True)
