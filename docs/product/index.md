# Kloudlite

Kloudlite shortens the development loop. The loop is *change → observe*; every
step between those two — build, push, deploy, set up, tear down — is overhead, and
Kloudlite removes it.

Three nouns describe the product:

- A [**workspace**](concepts/workspaces.md) is where you run things against your
  application: a warm sandbox with dependencies installed — a service you are
  developing, a test suite, a script.
- An [**environment**](concepts/environments.md) is where your application runs: every
  service, owned by one developer, cloneable, with all of its data capturable in a
  single [**snapshot**](concepts/snapshots.md).
- A [**connection**](concepts/connections.md) joins them. The workspace calls any
  service in the environment; **intercept** when you want the environment's traffic
  to reach your code instead of the deployed copy — no build, no deploy.

Most of these operations are issued by [agents](concepts/agents.md), in parallel,
on a developer's behalf.

## Start here

- **Why it works this way** — [Overview](concepts/overview.md), then the three
  nouns above.
- **Do it once** — [Your first workspace](tutorials/01-first-workspace.md).
- **Do it well** — [Best practices](best-practices.md).
- **Do a specific thing** — [How-to guides](how-to/).
- **Look something up** — [Reference](reference/).
