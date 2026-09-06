#!/usr/bin/env python3
"""Fail-fast watcher, run INSIDE the pod under pm2 (`pm2 start --name watch-<suite> ...`).

Reads the suite's log every 15 s. On the first failed step it stops the suite's pm2 process, closes
the run's row through the admin api and deletes the run's workspaces and environments through /v1
with the tenant's own token, then exits 1. Exits 0 when the run finishes with no failure.
  watch.py <suite> <tenant> <log> [--no-fail-fast]
"""
import sys, os, json, time, subprocess, hmac, hashlib, base64, urllib.request, datetime

suite, tenant, log = sys.argv[1], sys.argv[2], sys.argv[3]
fail_fast = "--no-fail-fast" not in sys.argv[4:]

def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
def token(sub, name, superadmin):
    now = int(time.time())
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
    claims = {"sub": sub, "name": name, "username": name, "typ": "session", "iat": now, "exp": now + 600}
    if superadmin: claims["superadmin"] = True
    c = b64(json.dumps(claims, separators=(",", ":")).encode())
    sig = b64(hmac.new(os.environ["KLOUDLITE_JWT_SECRET"].encode(), f"{h}.{c}".encode(), hashlib.sha256).digest())
    return f"{h}.{c}.{sig}"

def summarise():
    done, fails, n, skipped, run, last = 0, [], 0, 0, "", ""
    try: lines = open(log).read().splitlines()
    except FileNotFoundError: return None
    for l in lines:
        if "{" not in l: continue
        try: d = json.loads(l[l.index("{"):])
        except Exception: continue
        m = d.get("message")
        if m == "slo.run.started": run = d.get("run_id", "")
        if m == "slo.step.done":
            n += 1; last = d.get("slo_id")
            if not d.get("ok"): fails.append((d["timestamp"][11:19], d.get("slo_id"), d.get("ms"), str(d.get("detail", ""))[:260]))
        if m == "slo.step.skipped": skipped += 1
        if m == "slo.run.finished": done = 1
    return done, fails, n, skipped, run, last

def close_row(rid):
    H = {"authorization": "Bearer " + token("slo-probe@kloudlite.io", "slo-probe", True), "content-type": "application/json"}
    base = os.environ["KLOUDLITE_ADMIN_API_URL"].rstrip("/")
    try: d = json.load(urllib.request.urlopen(urllib.request.Request(f"{base}/admin/slo/runs/{rid}", headers=H), timeout=20))
    except Exception as e: print("row lookup failed:", e); return
    if d.get("state") != "running": print("row already", d.get("state")); return
    body = {"run_id": rid, "suite": d["suite"], "region": d["region"], "started": d["started"],
            "finished": datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%S.000Z"),
            "state": "failed", "stage": d["stage"] + " (killed by the operator: fail-fast)", "steps": d["steps"]}
    r = urllib.request.urlopen(urllib.request.Request(f"{base}/admin/slo/runs/{rid}", data=json.dumps(body).encode(), headers=H, method="PUT"), timeout=20)
    print("row closed ->", r.status)

def cleanup(rid):
    H = {"authorization": "Bearer " + token(f"{tenant}@kloudlite.io", tenant, False)}
    base = os.environ["KLOUDLITE_API_URL"].rstrip("/"); prefix = "run-" + rid; gone = 0
    for kind in ("workspaces", "environments"):
        try: rows = json.load(urllib.request.urlopen(urllib.request.Request(f"{base}/v1/{kind}?owner={tenant}", headers=H), timeout=20))
        except Exception as e: print("list", kind, "failed:", e); continue
        for r in (rows if isinstance(rows, list) else rows.get("items") or []):
            if str(r.get("name", "")).startswith(prefix):
                try: urllib.request.urlopen(urllib.request.Request(f"{base}/v1/{kind}/{r['id']}", headers=H, method="DELETE"), timeout=30); gone += 1
                except Exception as e: print("delete", kind, r.get("name"), "failed:", e)
    # A volume is named by its workspace id, not the run prefix; every detached one a probe
    # tenant still owns is a killed run's leftover and counts against the tenant's diskGb.
    try:
        rows = json.load(urllib.request.urlopen(urllib.request.Request(f"{base}/v1/volumes?owner={tenant}", headers=H), timeout=20))
        for r in (rows if isinstance(rows, list) else rows.get("items") or []):
            if r.get("deleted") and r.get("name"):
                try: urllib.request.urlopen(urllib.request.Request(f"{base}/v1/volumes/{r['name']}", headers=H, method="DELETE"), timeout=30); gone += 1
                except Exception as e: print("delete volume", r.get("name"), "failed:", e)
    except Exception as e: print("list volumes failed:", e)
    print(f"cleanup: {gone} objects of {prefix} (and detached volumes) deleted")

seen = 0
for _ in range(720):
    s = summarise()
    if s is None: time.sleep(15); continue
    done, fails, n, skipped, run, last = s
    line = f"{'DONE' if done else 'RUN'} | run: {run} | steps: {n} | skipped: {skipped} | last: {last} | fails: {len(fails)}"
    if n != seen: print(line, flush=True); seen = n
    if fails and fail_fast:
        print(line, flush=True); [print("  FAIL", *f, flush=True) for f in fails]
        subprocess.run(["pm2", "stop", suite], capture_output=True); subprocess.run(["pkill", "-x", "kloudlite-slo"])
        print("FAIL FAST: killed the run", flush=True)
        if run: close_row(run); cleanup(run)
        sys.exit(1)
    if done:
        print(line, flush=True); [print("  FAIL", *f, flush=True) for f in fails]; sys.exit(1 if fails else 0)
    time.sleep(15)
print("gave up waiting"); sys.exit(4)
