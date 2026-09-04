# Only Cloudflare may reach the HTTP origin

`dev.kloudlite.io` and `cr.khost.dev` are Cloudflare-proxied, so the ingress LoadBalancer's public
IP must accept 80/443 from Cloudflare's ranges only — otherwise an attacker (or a DDoS) skips the
edge and hits the origin directly, `CF-Connecting-IP` becomes forgeable, and everything that
trusts the real IP (the registry `limit-whitelist`, `KLOUDLITE_GIT_AGENT_SOURCES`) trusts a header
the client wrote.

The Azure NSG rule is written by the cloud controller from the Service, so the lock is
`spec.loadBalancerSourceRanges` on `svc/ingress-nginx-controller`. It is a committed manifest,
`deploy/ingress-nginx-service.yaml`, generated from `deploy/k3s/cloudflare-ips-v4.txt` by
`deploy/cf-sync.sh`; never patch the ranges by hand.

    kubectl apply --server-side --force-conflicts -f deploy/ingress-nginx-service.yaml

Server-side because the Service is Helm's: the manifest carries only the one field, and SSA adds
it without taking the rest of the object away from Helm. A Helm upgrade or reinstall of
ingress-nginx drops the field — re-apply after either, and before any roll check that

    kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.spec.loadBalancerSourceRanges}'

is non-empty and equal to the file. An empty list admits everyone. From outside Cloudflare,
`curl -m 5 http://<LB-IP>/` must time out.

When Cloudflare's ranges change, `.github/workflows/cf-sync.yml` goes red (weekly check). Then:
`deploy/cf-sync.sh`, commit what it rewrote, re-apply the Service and the ConfigMap, and follow
the script's printed steps for the two copies outside git (the pool NSG rule and `harden-node.sh`).
The git SSH LoadBalancer (`git.khost.dev`, not proxied) is a different Service and is untouched.
