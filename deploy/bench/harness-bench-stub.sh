#!/usr/bin/env bash
# Stub for the real harness-bench (harness/bench/dist/harness-bench, built by the other plan).
# Same interface: one bench per NODE_NAME lock at /bench/.lock, a 7789 listener that answers
# one connection at a time and dies of idleness, and a --ping the pod's readiness probe calls.
set -u

if [ "${1:-}" = "--ping" ]; then
  # ponytail: do not dial 7789 here — each connection restarts socat's idle `timeout` below, so
  # a 5s readiness probe that connected would keep an otherwise-idle bench pod alive forever.
  # Instead check locally that this process holds the lock and its listener is actually up.
  [ "$(cat /bench/.lock.holder 2>/dev/null)" = "${NODE_NAME:-}" ] \
    && pgrep -f 'socat TCP-LISTEN:7789' >/dev/null 2>&1
  exit $?
fi

mode=running
[ "${1:-}" = "--read-only" ] && mode=read-only

exec 9>/bench/.lock
if ! flock -n 9; then
  cat /bench/.lock.holder > /dev/termination-log 2>/dev/null
  exit 75
fi
echo "${NODE_NAME:-}" > /bench/.lock.holder

body="ok stub $mode"
len=${#body}

# Loop continues (exit 0) on every served connection, restarting the idle clock each time;
# `timeout` exits 124 only when no connection arrived for the whole period — that is the
# stub's "no client and nothing running", so only that case reports Idle. Any other socat
# failure exits non-zero here so the pod restarts instead of going Idle.
while timeout "${KL_BENCH_IDLE_SECS:-300}" socat TCP-LISTEN:7789,reuseaddr SYSTEM:"printf 'HTTP/1.1 200 OK\r\ncontent-length: $len\r\n\r\n$body'"; do
  :
done
status=$?
if [ "$status" -eq 124 ]; then
  echo idle > /dev/termination-log
  exit 0
fi
exit "$status"
