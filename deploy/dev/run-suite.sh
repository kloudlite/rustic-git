#!/usr/bin/env bash
# Run one probe suite from the dev pod, fail fast, and leave nothing behind.
#   deploy/dev/run-suite.sh weekly [--no-fail-fast]
# The suite runs as a process in the pod (nohup, log under /work/runs/). On the first failed step
# the process is killed, its `running` row is closed as failed through the admin api (or the next
# run of the suite yields to the ghost for ten minutes), and the failures are printed. Refuses to
# start while any SLO Job is active — a hand run and a scheduled one must never overlap.
set -euo pipefail
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
# `--attach /work/runs/<log>`: watch a run already started in the pod instead of starting one.
ATTACH=""; if [ "${1:-}" = "--attach" ]; then ATTACH=${2:?log path in the pod}; SUITE=attach; shift 2; fi
FF=1; [ "${1:-}" = "--no-fail-fast" ] && FF=0
[ -n "$ATTACH" ] || SUITE=${1:?fast|hourly|weekly|monthly}
if [ -z "$ATTACH" ]; then
case "$SUITE" in
  fast)    U=slo-probe;  O=slo-other;        K=/etc/slo-ssh-fast;   B=840  ;;
  hourly)  U=slo-hourly; O=slo-hourly-other; K=/etc/slo-ssh-hourly; B=3000 ;;
  weekly)  U=slo-drill;  O=slo-drill-other;  K=/etc/slo-ssh-drill;  B=6600 ;;
  monthly) U=slo-drill;  O=slo-drill-other;  K=/etc/slo-ssh-drill;  B=6900 ;;
  *) echo "unknown suite $SUITE" >&2; exit 2 ;;
esac
ACTIVE=$(kubectl -n kloudlite get jobs -o json | python3 -c 'import sys,json; print(" ".join(j["metadata"]["name"] for j in json.load(sys.stdin)["items"] if (j.get("status",{}).get("active") or 0)>0))')
[ -z "$ACTIVE" ] || { echo "refusing: probe Job(s) active: $ACTIVE" >&2; exit 3; }
LOG=/work/runs/$SUITE-$(date -u +%H%M).log
kubectl -n kloudlite exec "$POD" -- bash -c "mkdir -p /work/runs; ps -o stat= -C kloudlite-slo 2>/dev/null | grep -qv Z && { echo 'a suite is already running in the pod' >&2; exit 3; }; \
  cd /work/src && (nohup env KLOUDLITE_SLO_USER=$U KLOUDLITE_SLO_OTHER=$O KLOUDLITE_SLO_BUDGET_SECS=$B KLOUDLITE_SLO_SSH_KEY=$K/id_ed25519 \
  /work/target/dev-image/kloudlite-slo run --suite $SUITE 2>&1 | tee $LOG > /proc/1/fd/1 &); sleep 1; echo started $LOG"
else LOG=$ATTACH; echo "attached to $LOG"; fi
summarise() { kubectl -n kloudlite exec -i "$POD" -- python3 - "$LOG" <<'PY'
import sys,json
done=0; fails=[]; n=0; skipped=0; run=''; last=''
for l in open(sys.argv[1]):
    try: d=json.loads(l)
    except: continue
    m=d.get('message')
    if m=='slo.run.started': run=d.get('run_id','')
    if m=='slo.step.done':
        n+=1; last=d.get('slo_id')
        if not d.get('ok'): fails.append((d['timestamp'][11:19], d.get('slo_id'), d.get('ms'), str(d.get('detail',''))[:260]))
    if m=='slo.step.skipped': skipped+=1
    if m=='slo.run.finished': done=1
print('DONE' if done else 'RUN', '| run:', run, '| steps:', n, '| skipped:', skipped, '| last:', last, '| fails:', len(fails))
for f in fails: print('  FAIL', *f)
PY
}
close_row() { kubectl -n kloudlite exec -i "$POD" -- python3 - "$1" <<'PY'
import sys,os,hmac,hashlib,base64,json,time,urllib.request,datetime
rid=sys.argv[1]
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b'=').decode()
now=int(time.time()); h=b64(json.dumps({"alg":"HS256","typ":"JWT"},separators=(',',':')).encode())
c=b64(json.dumps({"sub":"slo-probe@kloudlite.io","name":"slo-probe","username":"slo-probe","typ":"session","superadmin":True,"iat":now,"exp":now+600},separators=(',',':')).encode())
tok=f"{h}.{c}."+b64(hmac.new(os.environ['KLOUDLITE_JWT_SECRET'].encode(),f"{h}.{c}".encode(),hashlib.sha256).digest())
H={"authorization":"Bearer "+tok,"content-type":"application/json"}; base=os.environ['KLOUDLITE_ADMIN_API_URL'].rstrip('/')
d=json.load(urllib.request.urlopen(urllib.request.Request(f"{base}/admin/slo/runs/{rid}",headers=H),timeout=20))
if d.get('state')=='running':
    body={"run_id":rid,"suite":d["suite"],"region":d["region"],"started":d["started"],"finished":datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%dT%H:%M:%S.000Z'),"state":"failed","stage":d["stage"]+" (killed by the operator: fail-fast)","steps":d["steps"]}
    print("row closed ->", urllib.request.urlopen(urllib.request.Request(f"{base}/admin/slo/runs/{rid}",data=json.dumps(body).encode(),headers=H,method="PUT"),timeout=20).status)
else: print("row already", d.get('state'))
PY
}
for i in $(seq 1 700); do
  S=$(summarise 2>/dev/null || true)
  # An empty summary is the exec failing, not the suite: never kill on it.
  [ -z "$S" ] && { sleep 15; continue; }
  case "$S" in
    DONE*) echo "$S"; exit 0 ;;
    *"fails: 0"*) ;;
    *) echo "$S"
       if [ "$FF" = 1 ]; then
         RUN=$(echo "$S" | head -1 | sed -n 's/.*run: \([^ ]*\).*/\1/p')
         kubectl -n kloudlite exec "$POD" -- pkill -x kloudlite-slo || true
         echo "FAIL FAST: killed the run"; [ -n "$RUN" ] && close_row "$RUN"
         exit 1
       fi ;;
  esac
  sleep 15
done
echo "gave up waiting"; exit 4
