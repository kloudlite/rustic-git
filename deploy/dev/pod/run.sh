#!/usr/bin/env bash
# Run one command under pm2 inside the pod and wait for it: `run.sh <name> <command...>`.
# Everything long lives in pm2 (`pm2 logs <name>`), and the caller still gets the exit code.
set -euo pipefail
NAME=${1:?name}; shift
pm2 delete "$NAME" >/dev/null 2>&1 || true
pm2 start --name "$NAME" --no-autorestart --time --merge-logs bash -- -c "$*" >/dev/null
while :; do
  S=$(pm2 jlist | python3 -c 'import sys,json; p=[x for x in json.load(sys.stdin) if x["name"]==sys.argv[1]]; print(p[0]["pm2_env"]["status"] if p else "gone"); print(p[0]["pm2_env"].get("exit_code","") if p else "")' "$NAME")
  STATUS=$(echo "$S" | head -1); CODE=$(echo "$S" | tail -1)
  case "$STATUS" in online|launching) sleep 3 ;; *) break ;; esac
done
pm2 logs "$NAME" --nostream --lines 40 --raw 2>/dev/null | tail -40
[ "$STATUS" = "stopped" ] && [ "${CODE:-1}" = "0" ] && exit 0
echo "[$NAME: $STATUS exit=$CODE]" >&2; exit 1
