#!/usr/bin/env bash
# Run a graft shim (hooks, statusline) or the graft MCP server IN THE DEV POD, against the checkout
# of record at /work/src, with this machine's hook JSON piped through. The laptop only pulls; it
# has no graph and needs no graft install. Inside the pod (where /work/src is the cwd's real path)
# the same script just runs the shim directly, so one settings.json serves both places.
#
#   graft-pod.sh hooks <event>   # .claude/helpers/graft-hooks.cjs <event>
#   graft-pod.sh statusline      # .claude/helpers/graft-statusline.cjs
#   graft-pod.sh mcp             # graft mcp (stdio)
#
# Never fails a session: any error (no kubectl, no pod, a timeout) is a silent no-op, exit 0.
set -u
kind=${1:-hooks}; event=${2:-}
here=${CLAUDE_PROJECT_DIR:-$(pwd)}
POD_SRC=/work/src

case "$kind" in
  hooks)      cmd="node $POD_SRC/.claude/helpers/graft-hooks.cjs $event" ;;
  statusline) cmd="node $POD_SRC/.claude/helpers/graft-statusline.cjs" ;;
  mcp)        cmd="graft mcp" ;;
  *) exit 0 ;;
esac

# In the pod: no bridge, the shim runs where the graph is.
if [ "$(cd "$here" 2>/dev/null && pwd -P)" = "$POD_SRC" ]; then
  exec bash -c "cd $POD_SRC && CLAUDE_PROJECT_DIR=$POD_SRC $cmd"
fi

command -v kubectl >/dev/null 2>&1 || exit 0
# The pod name, cached for a minute: the statusline asks several times a minute and a kubectl
# get per ask would dominate its cost.
cache=/tmp/graft-pod.name
if [ ! -s "$cache" ] || [ -n "$(find "$cache" -mmin +1 2>/dev/null)" ]; then
  kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}' > "$cache.tmp" 2>/dev/null \
    && mv "$cache.tmp" "$cache" || { rm -f "$cache.tmp"; exit 0; }
fi
pod=$(cat "$cache"); [ -n "$pod" ] || exit 0

# Paths in the hook JSON are this machine's; the pod knows them as /work/src. Mapped both ways so
# an edited file resolves in the pod and the answer's paths resolve here.
esc_here=$(printf '%s' "$here" | sed 's/[.[\*^$/]/\\&/g')
esc_pod=$(printf '%s' "$POD_SRC" | sed 's/[.[\*^$/]/\\&/g')
if [ "$kind" = mcp ]; then
  exec kubectl -n kloudlite exec -i -c dev "$pod" -- bash -c "cd $POD_SRC && DO_NOT_TRACK=1 $cmd"
fi
sed "s|$esc_here|$POD_SRC|g" \
  | kubectl -n kloudlite exec -i -c dev "$pod" -- bash -c "cd $POD_SRC && CLAUDE_PROJECT_DIR=$POD_SRC DO_NOT_TRACK=1 $cmd" 2>/dev/null \
  | sed "s|$esc_pod|$here|g"
exit 0
