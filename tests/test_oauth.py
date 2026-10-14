#!/usr/bin/env python3
"""OAuth flow test against nerve's public listener: discovery, DCR, PIN authorize, PKCE token, refresh, MCP call.
usage: python tests/test_oauth.py [public_base] --pin PIN
"""
import base64
import hashlib
import json
import secrets
import sys
import urllib.error
import urllib.parse
import urllib.request

BASE = next((a for a in sys.argv[1:] if a.startswith("http")), "http://127.0.0.1:8101").rstrip("/")
PIN = sys.argv[sys.argv.index("--pin") + 1]
FAILS = []


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


opener = urllib.request.build_opener(NoRedirect)


def req(method, path, data=None, headers=None, form=False):
    h = dict(headers or {})
    body = None
    if data is not None:
        if form:
            body = urllib.parse.urlencode(data).encode()
            h["Content-Type"] = "application/x-www-form-urlencoded"
        else:
            body = json.dumps(data).encode()
            h["Content-Type"] = "application/json"
    r = urllib.request.Request(path if path.startswith("http") else BASE + path, data=body, headers=h, method=method)
    try:
        resp = opener.open(r, timeout=30)
    except urllib.error.HTTPError as e:
        resp = e
    raw = resp.read().decode("utf-8", "replace")
    return resp.status, dict(resp.headers), raw


def check(label, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + label + ("" if cond else "  -> " + str(detail)[:500]))
    if not cond:
        FAILS.append(label)


st, hd, body = req("POST", "/mcp", {"jsonrpc": "2.0", "id": 1, "method": "ping"}, {"Accept": "application/json, text/event-stream"})
check("unauthenticated -> 401 + WWW-Authenticate", st == 401 and "resource_metadata" in hd.get("WWW-Authenticate", hd.get("www-authenticate", "")), (st, hd))
st, _, body = req("GET", "/.well-known/oauth-protected-resource")
check("protected resource metadata", st == 200 and "authorization_servers" in body, body)
st, _, body = req("GET", "/.well-known/oauth-authorization-server")
meta = json.loads(body)
check("AS metadata", st == 200 and meta.get("registration_endpoint") and "S256" in meta.get("code_challenge_methods_supported", []), body)
redirect = "http://127.0.0.1:9/callback"
st, _, body = req("POST", "/oauth/register", {"client_name": "oauth-test", "redirect_uris": [redirect], "token_endpoint_auth_method": "none"})
client = json.loads(body)
check("DCR", st in (200, 201) and client.get("client_id"), body)
cid = client["client_id"]
verifier = secrets.token_urlsafe(48)
challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
q = {"response_type": "code", "client_id": cid, "redirect_uri": redirect, "code_challenge": challenge, "code_challenge_method": "S256", "state": "xyz", "scope": "mcp"}
st, _, body = req("GET", "/oauth/authorize?" + urllib.parse.urlencode(q))
check("authorize page asks for PIN", st == 200 and "pin" in body.lower(), body[:300])
# wrong PIN
form = dict(q)
form.update({"pin": "0000-wrong", "decision": "allow"})
st, hd, body = req("POST", "/oauth/authorize", form, form=True)
check("wrong PIN rejected", st in (200, 400, 401, 403) and "code=" not in hd.get("Location", hd.get("location", "")), (st, hd.get("Location")))
import re
inputs = dict(re.findall(r'name="([^"]+)" value="([^"]*)"', body)) if body else {}
form = dict(q)
form.update(inputs)
form.update({"pin": PIN, "decision": "allow"})
st, hd, body = req("POST", "/oauth/authorize", form, form=True)
loc = hd.get("Location") or hd.get("location") or ""
check("correct PIN -> redirect with code", st in (302, 303) and "code=" in loc and "state=xyz" in loc, (st, loc, body[:300]))
code = urllib.parse.parse_qs(urllib.parse.urlparse(loc).query).get("code", [""])[0]
st, _, body = req("POST", "/oauth/token", {"grant_type": "authorization_code", "code": code, "redirect_uri": redirect, "client_id": cid, "code_verifier": "bad" + verifier}, form=True)
check("bad PKCE verifier rejected", st == 400, body)
st, _, body = req("POST", "/oauth/token", {"grant_type": "authorization_code", "code": code, "redirect_uri": redirect, "client_id": cid, "code_verifier": verifier}, form=True)
check("code burned after failed attempt", st == 400, body)
st, hd, body = req("POST", "/oauth/authorize", form, form=True)
loc = hd.get("Location") or hd.get("location") or ""
code = urllib.parse.parse_qs(urllib.parse.urlparse(loc).query).get("code", [""])[0]
st, _, body = req("POST", "/oauth/token", {"grant_type": "authorization_code", "code": code, "redirect_uri": redirect, "client_id": cid, "code_verifier": verifier}, form=True)
tok = json.loads(body) if body.startswith("{") else {}
# a code is single-use: if the bad attempt burned it, that's acceptable but report
check("token exchange", st == 200 and tok.get("access_token") and tok.get("refresh_token"), body)
if tok.get("access_token"):
    st, hd, body = req("POST", "/mcp", {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}, {"Accept": "application/json, text/event-stream", "Authorization": "Bearer " + tok["access_token"]})
    check("authenticated initialize", st == 200 and "nerve" in body, (st, body[:300]))
    st, _, body = req("POST", "/oauth/token", {"grant_type": "refresh_token", "refresh_token": tok["refresh_token"], "client_id": cid}, form=True)
    tok2 = json.loads(body) if body.startswith("{") else {}
    check("refresh rotation", st == 200 and tok2.get("access_token") and tok2.get("refresh_token") != tok["refresh_token"], body)
    st, _, body = req("POST", "/oauth/token", {"grant_type": "refresh_token", "refresh_token": tok["refresh_token"], "client_id": cid}, form=True)
    check("old refresh token invalid", st == 400, body)
print("\n%d failed" % len(FAILS) if FAILS else "\nALL PASSED")
sys.exit(1 if FAILS else 0)