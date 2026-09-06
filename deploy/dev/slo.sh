#!/usr/bin/env bash
# Run one probe suite from the dev pod with the CronJob's own tenant, key and budget.
#   deploy/dev/slo.sh fast|hourly|weekly|monthly
set -euo pipefail
SUITE=${1:?suite}
POD=$(kubectl -n kloudlite get pods -l app=dev -o jsonpath='{.items[0].metadata.name}')
case "$SUITE" in
  fast)    USER_=slo-probe;  OTHER=slo-other;         KEY=/etc/slo-ssh-fast;   BUDGET=840  ;;
  hourly)  USER_=slo-hourly; OTHER=slo-hourly-other; KEY=/etc/slo-ssh-hourly; BUDGET=3000 ;;
  weekly)  USER_=slo-drill;  OTHER=slo-drill-other;  KEY=/etc/slo-ssh-drill;  BUDGET=6600 ;;
  monthly) USER_=slo-drill;  OTHER=slo-drill-other;  KEY=/etc/slo-ssh-drill;  BUDGET=6900 ;;
  *) echo "unknown suite $SUITE" >&2; exit 2 ;;
esac
kubectl -n kloudlite exec "$POD" -- bash -lc "cd /work/src && KLOUDLITE_SLO_USER=$USER_ KLOUDLITE_SLO_OTHER=$OTHER KLOUDLITE_SLO_BUDGET_SECS=$BUDGET \
  KLOUDLITE_SLO_SSH_KEY=$KEY/id_ed25519 kloudlite-slo run --suite $SUITE"
