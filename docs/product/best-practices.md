# Best Practices

Everything here follows from one rule: **the loop is change → observe, and anything
between those two steps is overhead.** If a habit reintroduces a step Kloudlite
removed, drop the habit.

## Workspaces

**Keep one long-lived workspace per component you own.** Its value is being warm.
Do not recreate it per task; [clone](concepts/workspaces.md#ephemeral-workspaces)
from it instead.

**Size to the change, not the architecture.** One service per workspace is the
default because separate services rarely want to share a dependency tree. Widen it
only for code you always change together.

**Only make workspaces for what you bring.** The component you are editing, the
test suite you are running, the script you are executing. Everything else runs in
the environment. If you find yourself creating a workspace for a component just to
have it running, you wanted a [connection](concepts/connections.md), not a
workspace.

**A test suite is a workspace too.** Keep the integration tests in their own
workspace, connected to a fresh environment clone per run. It is a client of the
application, not part of it — nothing to intercept, nothing to deploy.

**Treat every workspace as reconstructible.** Ephemeral workspaces are destroyed by
design; long-lived ones should be re-creatable from the repository plus a setup
step. Nothing that matters lives only in a workspace. Push.

**Install tools into the workspace, not into a new workspace.** A missing tool is
a Nix package away and needs no restart. Recreating or rebuilding a workspace to
add a tool throws away the warmth for nothing.

**Declare the tools a component always needs; install the rest ad hoc.** The
runtime, the package manager, the test runner belong to the workspace's definition
so every clone has them. A one-off client for a debugging session does not.

**Do the work with exec.** Build, test, run, migrate — all commands. If a task needs
more than read, write, and exec, question the task.

**Run the component in the background, under a reloader.** Start it once, let it
pick up saves. Relaunching per check is a build step in disguise. Run a test watcher
next to it so every save reports twice — from the traffic and from the tests.

## Intercepts

**Run first; intercept when you need inbound traffic.** A connected workspace can
already call every service. Intercept only when the thing you need to see is
other services calling you — a request hitting your handler, a webhook, a queue consumer.

**Leave it on for the session, not per request.** Intercept is a mode you work in.
Release it when you are done with the component, not between edits.

**Intercept only in an environment you own — or with the owner's knowledge.**
Interception takes all of a component's traffic. In someone else's environment it
takes theirs.

**Release intercepts when you stop.** A workspace that dies leaves a
[dead intercept](concepts/connections.md#when-the-connection-ends) on the
component's status. Harmless to the environment, confusing to the next person who
wonders why the service behaves like the deployed version.

**When a service does the wrong thing, check the intercept before the code.** Is it
intercepted? By which workspace? Is that workspace alive? Most "my change has no
effect" reports end there.

## Environments

**One per developer. Never a shared "dev".** Sharing is the thing environments
exist to make unnecessary. If two people need the same application, one clones it.

**Clone, do not fix.** An environment that has drifted into a bad state is cheaper
to discard and re-clone than to repair. Treat environments as disposable and keep
what matters in [snapshots](#snapshots-and-testing).

**Keep a stable one and a working one.** Take apart the working one freely; keep
the stable one as the thing you clone from and compare against.

**Connect to a teammate's environment to collaborate; do not ask them to deploy.**
Your workspace, their environment. It is faster and it leaves their environment exactly
as it was when you disconnect.

## Snapshots and testing

**Snapshot states worth returning to, and name them for what they are.**
`pre-migration-orders-v2`, `bug-4812-repro`, `demo-seed`. A snapshot named by date
tells you nothing when you need it.

**Never hand-mutate an environment into a test state.** Snapshot the state once,
then clone from it every time. If you are running a seed script, you are
reconstructing what a snapshot would have kept.

**One clone per test run. Always.** Concurrent runs on a shared environment are the
problem snapshots solve. Do not solve it a second way.

**Delete your teardown.** A test that ends with discarding its clone has no cleanup
to write, no cleanup to fail, and no dirty state to leak into the next run.

**Let tests mutate freely.** Tests written to avoid touching data — read-only
assertions, careful rollbacks — were written for shared environments. On a clone,
the test may do anything.

**Assume nothing from a previous test.** Every clone starts from the snapshot.
Ordering dependencies between tests are now bugs, not conventions.

## Agents

**One ephemeral workspace per task. No exceptions.** Two agents in one tree is the
laptop problem at machine speed.

**Push before discard, and discard.** The cycle is clone → work → commit → push →
discard. An agent that leaves ephemeral workspaces behind leaves warmth it does not
need and confusion for whoever finds them.

**A task that mutates data gets its own environment clone.** Verifying against the
developer's environment is fine for reads. Anything that writes, migrates, or
queues gets a clone from a snapshot, then discards it.

**Scope a task to one workspace.** If a task needs coordinated edits across several
components, split it into per-component tasks, or accept that it runs serially. An
agent that has to coordinate across workspaces is reconstructing the coupling the
workspace boundary was meant to remove.

**Verify in the loop, not after it.** The point of a warm workspace connected to a
real environment is that the agent can run the code against the application during the
task. Do not batch verification into a later pipeline stage that rebuilds the
context the agent already had.

**Let dead intercepts die.** An agent's workspace can be killed at any point. The
environment recovers by itself; the agent's job is to have pushed, not to have
cleaned up.

## Things to stop doing

| Habit | Why it is now wrong | Do instead |
|---|---|---|
| Building an image to test a change | That is the step intercept removes | Intercept and restart the process |
| A shared staging environment | Sharing is what per-developer environments replace | Clone one each |
| Seed scripts and fixtures for test state | Reconstructs what a snapshot keeps | Snapshot once, clone per run |
| Teardown scripts | Nothing to tear down when clones are discarded | Discard the clone |
| Running mutating tests serially | Only necessary when tests share state | One clone per run, in parallel |
| Recreating a workspace per task | Throws away the warmth that makes it valuable | Clone an ephemeral workspace |
| Asking a teammate to deploy your branch | Introduces the deploy step for them | Connect your workspace to their environment |
| A local docker-compose stack | A worse copy of the environment you already have | Connect to the environment |
