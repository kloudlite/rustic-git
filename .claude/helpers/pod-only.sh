#!/usr/bin/env bash
# PreToolUse: the checkout of record is /work/src in the dev pod, and the laptop only pulls.
# Inside the pod every tool runs as it is. Anywhere else, an Edit or Write on a file under this
# repository is APPLIED IN THE POD by this hook and the local tool call is then refused with
# "applied in the pod" — the agent keeps working, the laptop tree stays untouched until the next
# pull. Files outside the repository (memory, scratch) are left to the local tool. A Bash command
# that would run cargo here is refused unless it goes through the pod. Never wedges a session: any
# failure to read the input allows.
set -u
here=${CLAUDE_PROJECT_DIR:-$(pwd)}
[ "$(cd "$here" 2>/dev/null && pwd -P)" = "/work/src" ] && exit 0
command -v python3 >/dev/null 2>&1 || exit 0
exec python3 "$(dirname "$0")/pod-only.py" "$here"
