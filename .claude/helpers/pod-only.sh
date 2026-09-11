#!/usr/bin/env bash
# PreToolUse guard: the checkout of record is /work/src in the dev pod. On any other machine an
# Edit/Write/MultiEdit/NotebookEdit is refused, and so is a Bash command that runs cargo here —
# the laptop only pulls. Inside the pod (the project dir's real path is /work/src) everything is
# allowed. Never breaks a session: an unreadable input allows.
set -u
here=${CLAUDE_PROJECT_DIR:-$(pwd)}
[ "$(cd "$here" 2>/dev/null && pwd -P)" = "/work/src" ] && exit 0
input=$(cat 2>/dev/null || true)
tool=$(printf '%s' "$input" | sed -n 's/.*"tool_name":"\([^"]*\)".*/\1/p' | head -1)
deny() {
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$1"
  exit 0
}
case "$tool" in
  Edit|Write|MultiEdit|NotebookEdit)
    deny "This is the laptop clone. Edit the file in the dev pod (/work/src) instead: pipe a script through deploy/dev/exec.sh or kubectl exec -i, commit and push from there, and git pull here." ;;
  Bash)
    cmd=$(printf '%s' "$input" | sed -n 's/.*"command":"\(.*\)","description".*/\1/p' | head -1)
    [ -z "$cmd" ] && cmd=$(printf '%s' "$input" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | head -1)
    case "$cmd" in
      *kubectl\ exec*|*exec.sh*|*ship.sh*|*sync.sh*|*test.sh*) exit 0 ;;
      *cargo\ *|cargo|*cargo*) deny "Never run cargo on the Mac: build and test in the dev pod (deploy/dev/exec.sh 'cargo …')." ;;
    esac ;;
esac
exit 0
