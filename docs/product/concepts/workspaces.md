# Workspaces

A workspace is where you run things against your application.

It is a sandbox: source, installed dependencies, toolchain, and caches, in one
place that is ready to run the moment you open it. Most often it holds a service
you are developing; it can equally hold a test suite, a script, or a tool. The
application itself runs in an [environment](environments.md); the workspace
[connects](connections.md) to it.

## The gap it closes

Before you can fix the refund bug in `payments`, someone has to make a place to
fix it. Clone the repository. Install the right runtime. Install the dependencies.
Set the environment variables. Discover the undocumented step. Every developer
repeats this per machine, and an agent would repeat it per task.

A workspace is that place, made once and kept warm. Its value is not that it exists
but that it is *ready*: the dependencies are installed, the caches are populated,
the first build is already done. That is why workspaces are long-lived, and why the
way to get another one is to [clone](#ephemeral-workspaces) it rather than build it
from scratch.

## What goes in one

A workspace holds whatever you need to run against the environment. That is
often a service you are changing, but not always:

- **A component.** A `payments` workspace holds `payments` and its toolchain. You
  edit there, and [intercept](connections.md) when you want the shop's traffic to
  reach your code.
- **A test suite.** A QA engineer keeps a workspace holding the integration tests
  and nothing else. It connects to an environment, drives `web` and `api` end to
  end, and asserts on what lands in Postgres. Nothing is intercepted; the
  workspace is a client of the shop, not part of it.
- **Scripts and tools.** A workspace with a database client, a load generator, a
  migration runner, or a one-off script that reads from the queue. Anything that
  needs to sit inside the environment's network and be ready to run.

How much goes in one is your decision. One service per workspace is the common
shape for development, because `payments` and `api` have separate dependency trees
and separate test commands. But a workspace can hold a whole repository, or `api`
and `worker` together if they always change together. Kloudlite does not impose
a granularity.

These docs say **component** when the workspace holds a service that can be
intercepted. Read it as "the thing this workspace is a sandbox for".

## You only need workspaces for what you bring

The environment runs the whole shop — `web`, `api`, `payments`, `worker`, Postgres,
the queue. A workspace is for what you add to that: the component you are
changing, the tests you are running, the script you are executing. You do not need
a sandbox for `api` to have `api` running; it is already running in the
environment, and your workspace reaches it as it is.

So a developer fixing `payments` has one workspace. A QA engineer running the
integration suite has one workspace. A developer whose fix touches `payments` and
`api` has two — because they are editing two, not because the shop has six.

## Packages

A workspace manages its own tools. Anything it needs — a language runtime, a
database client, a linter, `jq`, a load generator — is installed as a Nix package
and is available in the workspace immediately, without restarting it.

This is what keeps a workspace warm across changes in what it needs. Working on
`payments` and discover you need the Postgres client to inspect a row? Install
it; it is on the path in the same shell, with `payments` still running in the
background and the intercept still live. Nothing is rebuilt and nothing is
restarted, because the workspace is not an image — it is a running sandbox whose
toolset can grow while it runs.

Nix also means the toolset is exact and reproducible. The package a workspace
installs is the same package, at the same version, on every workspace and every
clone. An ephemeral workspace cloned from `payments` carries the same tools, and
an agent that installs a tool mid-task gets a known version rather than whatever
a package mirror served that day.

For an agent this removes a whole class of stalls. A missing tool is an exec
away, not a reason to rebuild the sandbox or ask a human.

## The surface

A developer gets a shell. Open one in the workspace and it is a terminal like any
other: edit, run the tests, start the service in a second tab and leave it going.

An agent gets the same workspace through three operations:

- **Read** — list and read files.
- **Write** — create and modify files.
- **Exec** — run a command and get its output.

That is the whole agent surface, by design. Installing, building, testing, running
a migration — each is a command to exec. An agent needs nothing beyond these three
to do what a developer does at a terminal, with one addition.

### Background processes (for agents)

Not every command should block until it finishes. A workspace can run processes in
the background and keep them running: the component itself under a hot reloader, a
test watcher, a queue consumer.

This is the agent's second tab. A developer with a shell starts `payments`, leaves
it running, and edits in another tab. An agent with only a blocking exec cannot: it
either runs `payments` and hangs, or relaunches it for every check and pays the
startup cost each time.

The right shape for an agent working on the refund bug is:

1. Start `payments` in the background under a reloader.
2. Start the test watcher in the background next to it.
3. Spend the rest of the task on read, write, and exec against an application that is
   already running. Each save is picked up by the reloader; each check is a request
   or a test result, not a relaunch.

This is also what makes [intercepting](connections.md) a mode rather than a moment:
with `payments` running in the background and the shop's traffic routed to it, the
loop is *save the file, see the request*.

## Ephemeral workspaces

Now suppose you hand an agent five tickets against `payments` at once. The agent
does not work in your `payments` workspace directly — five tasks in one tree would
collide. It **clones an ephemeral workspace** from it, one per ticket.

An ephemeral workspace is a git worktree with the warmth included. It branches off
the parent, carrying its state and its installed dependencies, so it is ready to
build immediately. Five can exist from the same parent at once, each on its own
ticket, none touching the others.

The cycle is always the same: clone from the parent, do the task, commit, push to
the working branch, discard. Nothing survives the ephemeral workspace except the
pushed commits. Anything not pushed is gone when it is discarded — which is the
point, not a hazard. See [Agents](agents.md).

## Lifecycle

Both kinds of workspace share four operations:

- **Create** — a new workspace for a component, from a repository and a starting
  point.
- **Update** — change the workspace in place, keeping its state.
- **Clone** — a new workspace from an existing one, carrying its state and warm
  dependencies. This is how ephemeral workspaces come into being.
- **Discard** — destroy the workspace and everything unpushed in it.

Discard is routine for ephemeral workspaces and rare for the long-lived ones they
came from.

## Where next

- [Connections](connections.md) — reaching the shop, and intercepting.
- [Best practices: workspaces](../best-practices.md#workspaces)
- [Workspace API](../reference/api/workspaces.md)
