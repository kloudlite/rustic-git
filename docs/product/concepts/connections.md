# Connections and Intercepts

A [workspace](workspaces.md) holds the code you are changing. An
[environment](environments.md) runs the application it belongs to. A connection joins
them, so the two become one running whole without either moving.

## The gap it closes

Between editing a line in `payments` and seeing it run, there is normally an
artifact: an image to build, push, deploy, and roll out. The artifact exists
because we assume the code and the application must be in the same place, so the code
is packaged and shipped to where the application is.

A connection removes the assumption. The shop stays where it runs. Your code stays
in your workspace. The network joins them. With nothing to ship, there is nothing
to build.

A connection gives you one thing always, and one thing on request.

## Reaching the application (always)

Once connected, your workspace can address every service in the environment by
the same name the deployed components use. Your `payments` code calls `api` as
`api`, and Postgres as `postgres`, exactly as it would if it were deployed.
Nothing is mocked and nothing is port-forwarded by hand.

This covers the components you are not working on — which is most of them. They
run in the environment; you use them.

At this point the environment is untouched. It still runs its own copy of
`payments`, and the connection is one-way: your workspace calls the shop, the shop
does not call your workspace. For most work — running the tests against real
dependencies, hitting `api` from a script — this is all you need.

## Intercepting (on request)

Intercept when you need the traffic to come to you.

You want to trigger a refund from `web` and watch it arrive in your code. You
intercept `payments`. Kloudlite switches off the environment's `payments`
workload and routes its traffic to the process running in your workspace. When
`api` calls `payments`, it now reaches your code — still addressing it as
`payments`, unaware anything changed. The rest of the shop is untouched.

This is the step that deletes the build-and-deploy cycle. Save the file; the
reloader picks it up; the next refund request from `web` hits your three lines.
Attach a debugger to the process in your workspace and step through a request
that came from the real `api` with real data from Postgres.

Interception takes **all** `payments` traffic — it is not a slice of requests.
That is why an environment has a single owner, and why you intercept in someone
else's environment only with their knowledge.

Developers commonly leave a component intercepted for as long as they are working
on it. It is a mode to work in, not a switch to flip per request. Release it when
you are done, or when you want the environment back on the deployed version.

## Connecting elsewhere

A workspace can be disconnected from one environment and connected to another,
with the same source code — to test against different data, or to intercept
inside a teammate's environment so they can see your work running. See
[One workspace, different environments](environments.md#one-workspace-different-environments).

## When the connection ends

Interception is not a commitment. If your workspace disconnects, stops, is killed,
or is deleted while `payments` is intercepted, the environment's own `payments`
workload comes back and resumes serving.

The return is non-disruptive. `api` kept calling `payments` by name the whole time,
so the shop falls back to the deployed version with no action from anyone and
nothing left broken. A closed laptop or a crashed agent degrades to a normal
environment, never to a hole in the application.

The environment does not pretend it never happened. The `payments` component's
status reports a **dead intercept**: an interception still registered whose
workspace is gone. Traffic is being served by the deployed copy again, and the
status tells you there is a stale intercept to reconnect or clear. So when
`payments` suddenly behaves like the deployed version instead of like your code,
the reason is visible rather than puzzling.

This is also what makes [ephemeral workspaces](workspaces.md#ephemeral-workspaces)
safe to discard in bulk: an agent's five workspaces can vanish and the environment
tidies itself.

## Where next

- [Best practices: intercepts](../best-practices.md#intercepts)
- [Intercept a component](../how-to/connections/intercept.md)
- [A dead intercept is shown on a component](../how-to/troubleshooting/dead-intercept.md)
