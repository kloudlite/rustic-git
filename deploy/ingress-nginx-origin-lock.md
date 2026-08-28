# Only Cloudflare may reach the HTTP origin

`dev.kloudlite.io` and `cr.khost.dev` are Cloudflare-proxied, so the ingress LoadBalancer's public
IP must accept 80/443 from Cloudflare's ranges only — otherwise an attacker (or a DDoS) skips the
edge and hits the origin directly, and `CF-Connecting-IP` could be forged.

The Azure NSG rule is written by the cloud controller from the Service, so it is set there, not
in the NSG:

    kubectl -n ingress-nginx patch svc ingress-nginx-controller --type merge \
      -p '{"spec":{"loadBalancerSourceRanges":[<https://www.cloudflare.com/ips-v4 + ips-v6>]}}'

Refresh when Cloudflare's ranges change (they publish them; a stale list drops traffic from a new
edge rather than opening anything). `deploy/ingress-nginx-config.yaml` carries the same v4 list
for real-IP trust — keep the two in step. The git SSH LoadBalancer (`git.khost.dev`, not proxied)
is a different Service and is untouched.
