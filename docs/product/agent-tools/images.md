# Build and push images

A workspace has no container engine. `kl build` sends the build to a hidden per-owner builder and pushes the result to your registry as it finishes. `kl push` copies an image the registry already holds to another name.

## Build

```bash
cd ~/workspaces/api
kl build -t api:1 .
```

`api:1` means `cr.khost.dev/{you}/api:1`. `team/api:1` pushes under a team you belong to.

| Flag | Meaning |
|---|---|
| `-t, --tag NAME[:TAG]` | Required; may repeat |
| `-f, --file PATH` | Dockerfile; default `./Dockerfile` |
| `--build-arg KEY=VALUE` | May repeat |
| `--platform` | Target platform |
| `--no-cache` | Ignore the builder's layer cache |
| `CONTEXT` | Build context; default `.` |

## Push a tag

```bash
kl push api:1 api:latest
```

A registry-side copy: nothing is downloaded to the workspace.

## Where the image goes

`cr.khost.dev/{owner}/{name}:{tag}`. Use it in an [environment service](../environments/services.md) directly:

```json
{ "name": "api", "image": "cr.khost.dev/acme/api:1", "ports": [8080] }
```

## The builder

Each owner has one builder, started on the first build and stopped after idle time. It is never listed and never started by hand. When a build is waiting, ask why:

```bash [kl-connect]
kl-connect builder status
kl-connect builder status --team acme
```

```
state: creating
ready: false
why: buildkit not ready yet
```

Or `GET /v1/builders/me?team=acme` for the same as JSON (`id`, `state`, `ready`, `conditions`).

## Credentials

The workspace carries a registry token for the account it belongs to, refreshed on a beat and never shown on a command line. A team's builder pushes under the team's name with a member's credential; membership is what authorizes it.

## What does not work

`docker run`, `docker pull`, `docker ps`, and a separate `docker push` do not apply: there is no local image store. A build pushes as it finishes.
