#!/usr/bin/env bash
# rsync remote shell over kubectl exec: rsync calls this as `krsh.sh <pod> <command...>`.
POD=$1; shift
exec kubectl -n kloudlite exec -i "$POD" -- "$@"
