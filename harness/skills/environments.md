---
name: environments
description: Use when a service (database, queue, web) must exist, change, be reached by name, or its traffic sent to a workspace
---

# Environments

An environment runs the services your code talks to: a database, a queue, a web service. It has a
name, and inside it every service answers on its own name — `mongodb://db:27017` works because `db`
is a service in the same environment.

A space (you, or your team) follows ONE environment: `kl_env_current`, `kl_env_switch`,
`kl_env_clear`. Every workspace in that space resolves its services.

Verbs: `kl_environments`, `kl_environment`, `kl_environment_create` (a services list, or
`from_snapshot`), `kl_environment_start`, `kl_environment_stop`, `kl_environment_delete`,
`kl_environment_service_add`, `kl_environment_service_rm`, `kl_intercept`.

An intercept points one service's traffic at a workspace: everything that dials `api:8080` reaches
your dev server instead, and ports may be remapped. It holds until you clear it.

Example — add NATS, then take the api service over:

    kl_environment_service_add {id: "devstack", service: {name: "nats", image: "nats:2", ports: [4222]}}
    kl_intercept {id: "devstack", service: "api", workspace: "svelte-backend", ports: [{from: 8080, to: 3000}]}
