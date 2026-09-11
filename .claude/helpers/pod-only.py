# The Python half of pod-only.sh; a file, not a heredoc, so the hook JSON stays on stdin.
import json, os, subprocess, sys
here = os.path.realpath(sys.argv[1])
try:
    hook = json.load(sys.stdin)
except Exception:
    sys.exit(0)
tool = hook.get("tool_name", "")
inp = hook.get("tool_input", {}) or {}

def deny(reason):
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}))
    sys.exit(0)

def pod():
    cache = "/tmp/graft-pod.name"
    try:
        if os.path.getsize(cache) > 0 and os.path.getmtime(cache) > __import__("time").time() - 60:
            return open(cache).read().strip()
    except OSError:
        pass
    name = subprocess.run(["kubectl", "-n", "kloudlite", "get", "pods", "-l", "app=dev", "-o", "jsonpath={.items[0].metadata.name}"], capture_output=True, text=True, timeout=20).stdout.strip()
    if name:
        open(cache, "w").write(name)
    return name

def in_pod(script, payload):
    p = pod()
    if not p:
        deny("The dev pod could not be found; nothing was written. Fix kubectl access and retry.")
    r = subprocess.run(["kubectl", "-n", "kloudlite", "exec", "-i", "-c", "dev", p, "--", "python3", "-c", script], input=json.dumps(payload), capture_output=True, text=True, timeout=60)
    return r.returncode, (r.stdout + r.stderr).strip()

def pod_path(local):
    real = os.path.realpath(local)
    if real == here or real.startswith(here + os.sep):
        return "/work/src" + real[len(here):]
    return None

if tool in ("Edit", "Write", "MultiEdit"):
    target = pod_path(inp.get("file_path", ""))
    if target is None:
        sys.exit(0)  # outside the repository: the local tool may have it
    if tool == "Write":
        code, out = in_pod("""
import json,os,sys
d=json.load(sys.stdin); os.makedirs(os.path.dirname(d['p']), exist_ok=True)
open(d['p'],'w').write(d['c']); print('written', d['p'])""", {"p": target, "c": inp.get("content", "")})
    else:
        edits = inp.get("edits") if tool == "MultiEdit" else [{"old_string": inp.get("old_string", ""), "new_string": inp.get("new_string", ""), "replace_all": inp.get("replace_all", False)}]
        code, out = in_pod("""
import json,sys
d=json.load(sys.stdin); p=d['p']
try: s=open(p).read()
except FileNotFoundError: print('no such file in the pod:', p); sys.exit(1)
for e in d['edits']:
    n=s.count(e['old_string'])
    if n==0: print('old_string not found in the pod copy of', p); sys.exit(1)
    if n>1 and not e.get('replace_all'): print('old_string matches', n, 'places in', p, '- add context or replace_all'); sys.exit(1)
    s=s.replace(e['old_string'], e['new_string']) if e.get('replace_all') else s.replace(e['old_string'], e['new_string'], 1)
open(p,'w').write(s); print('edited', p)""", {"p": target, "edits": edits})
    if code == 0:
        deny("Applied in the dev pod instead of here: " + out + ". The pod checkout is the one to build, test, commit and push from; the laptop pulls.")
    deny("Not applied — the pod said: " + out)
elif tool == "NotebookEdit":
    deny("Notebook edits are not bridged to the dev pod; edit it there.")
elif tool == "Bash":
    cmd = inp.get("command", "")
    if any(k in cmd for k in ("exec.sh", "ship.sh", "sync.sh", "test.sh")) or ("kubectl" in cmd and " exec " in cmd):
        sys.exit(0)
    import re
    if re.search(r"(^|[\s;&|(])cargo(\s|$)", cmd):
        deny("Never run cargo on the Mac: build and test in the dev pod (deploy/dev/exec.sh 'cargo …').")
sys.exit(0)
