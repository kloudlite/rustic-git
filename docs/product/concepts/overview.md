# Overview

The development loop is two steps: **change something, then observe what it
does.** Everything else is overhead.

## An ordinary afternoon

Take a small online shop. It has a `web` frontend, an `api`, a `payments` service,
a background `worker`, a Postgres database, and a queue between `api` and
`worker`. Six pieces, all talking to each other.

You need to fix a bug in `payments`: refunds over a certain amount are rejected.
The fix is three lines. Here is what it costs to see those three lines work:

1. Build a new `payments` image. Push it to the registry.
2. Deploy it to the staging environment. Wait for the rollout.
3. Discover that staging has no order large enough to refund. Seed one by hand.
4. Trigger the refund from `web`, watch the logs, find a typo.
5. Go back to step 1.
6. Once it works, delete the test order so the next person's tests do not trip
   over it — and hope nobody else was using staging at the same time.

The change took a minute. The loop around it took the afternoon, and most of it
was waiting, setting up, and cleaning up. Nothing in steps 1–6 was the bug.

Kloudlite exists to delete those steps. Not to make them faster — to remove them.

## Why the loop is long

Two assumptions put the overhead there.

**Assumption one: your code and the running application have to be in the same place.**
Every familiar approach obeys it and only differs in which one it moves. Running
the shop on a laptop with docker-compose moves the application to the code — and the
laptop copy is never quite the real thing. Deploying moves the code to the application —
correct, but every iteration is a build and a rollout, and what comes back is a log
line instead of a debugger.

**Assumption two: state is precious.** Starting six containers is easy. What makes
staging scarce is the data inside it — the seeded orders, the customer records,
the queue that must not be left half-drained. So teams share one staging
environment, take turns with tests that write, and maintain cleanup scripts.

Nearly every ritual of modern development — build pipelines, shared staging,
fixtures, teardown jobs — is a workaround for one of those two assumptions.

## What Kloudlite does instead

**It connects instead of co-locating.** The shop keeps running where it runs. Your
`payments` code stays in your [workspace](workspaces.md). The two are joined over
the network, and when you [intercept](connections.md), the running shop sends its
`payments` traffic to your workspace instead of to the deployed copy. Save the
file; the next refund request hits your three lines. Nothing was built, because
nothing had to be shipped.

**It copies state instead of preparing it.** All of the shop's data — Postgres, the
queue, every volume — can be captured as one [snapshot](snapshots.md) and cloned
back in one step. Clone an environment from a snapshot that already contains a
large order, run the refund, discard the clone. No seeding, no cleanup, and nobody
else's tests were ever at risk.

Everything else in Kloudlite follows from those two moves.

## The gaps, and what closes them

| The gap | What normally fills it | Kloudlite |
|---|---|---|
| Between the edit and it running | build, push, deploy, rollout | [intercept](connections.md) |
| Between you and a realistic application | a laptop stack, or a turn on staging | an [environment](environments.md) you own |
| Between a test and the data it needs | seed scripts, fixtures, teardown | [snapshot and clone](snapshots.md) |
| Between two tasks at once | one working tree, stash, rebuild | [ephemeral workspaces](workspaces.md#ephemeral-workspaces) |
| Between the tools that hold the loop | credentials, webhooks, dashboards | [one platform](#the-whole-loop-in-one-place) |

## The three nouns

- A [**workspace**](workspaces.md) is where you run things against your application — a
  warm sandbox with dependencies installed. You have one for `payments`; a QA
  engineer has one for the integration suite.
- An [**environment**](environments.md) is where your application runs — the whole shop,
  owned by one developer. You have your own.
- A [**connection**](connections.md) joins them. Your workspace can call `api` or
  Postgres by name; and when you **intercept**, the shop's `payments` traffic comes
  to you.

Read those three pages in order and the product is fully described.

## The whole loop in one place

Kloudlite also hosts [git repositories](git-repositories.md) — with pull requests,
issues, and CI/CD — and [container repositories](container-repositories.md).

The reason is the same: gaps. The refund bug is an issue; the fix is a branch and a
pull request; the merged result is an image; the image runs in an environment with
data in it. When those live in five products, someone has to carry context between
them — credentials, webhooks, a tab per dashboard. When they live in one, the platform
already knows that this workspace is working on that issue and will push to that
branch. What makes the loop short is not any single feature but the absence of
seams between them.

## Why this matters now

The loop was always too long. Agents make it intolerable.

Suppose you hand the refund bug to an agent, along with four other tickets.

- **Agents fan out where people take turns.** You work on one thing at a time and
  can live with one working tree and one environment. Five agents at once cannot.
  Each needs its own sandbox and its own copy of the data, or they trample each
  other.
- **Waiting costs tokens.** A four-minute rollout is not just slow for an agent; it
  is state to hold, poll, and re-establish, paid on every iteration.
- **The glue has to be machine-readable.** You can reconcile a git host, a
  registry, and a cluster by eye. An agent needs one platform that already knows how
  the issue, the branch, the image, and the environment relate.
- **Nobody is watching.** If an agent's sandbox dies mid-task, the shop has to go
  back to serving `payments` from the deployed copy on its own.

Kloudlite is what a development platform looks like when you assume the users are
often not people, and are often many at once. Developers get the same benefits;
they were always paying these costs, just quietly.

## The point

Developer time and agent tokens should go to the problem — the three-line fix — not
to the afternoon around it. The loop should be what it always should have been:
change the code, see it run.

Next: [Workspaces](workspaces.md) → [Environments](environments.md) →
[Connections](connections.md) → [Best practices](../best-practices.md).
