# Limits and defaults

| | Value |
|---|---|
| Workspace name | 1–63 characters of `[A-Za-z0-9._-]`, unique per owner |
| Service name | DNS label, ≤ 63 characters |
| Default disk (`quota_gb`) | 20 GB for a workspace, 20 GB for an environment |
| Default image | `ghcr.io/kloudlite/kloudlite-workspace` |
| Package pin forms | `attr`, `attr@latest`, `attr@N`, `attr@N.N`, `attr@N.N.N` |
| Package lock cache | 24 hours |
| Quota, person | 5 workspaces · 2 environments · 20 snapshots · 100 GB · 40 cpu · 80 GB memory |
| Quota, team | 20 workspaces · 8 environments · 80 snapshots · 400 GB · 148 cpu · 296 GB memory |
| Pending requests | One per owner per kind |
| Intercept grace | 30 s of an unreachable workspace before the real service comes back |
| Node considered dead | 180 s NotReady |
| Key and membership propagation | Immediate on change; resynced every 300 s |
| ssh user | `kl` |
| Tree path | `/home/kl/workspaces/{name}` |
| Home | `/home/kl`, shared per person per region |
| Registry | `cr.khost.dev/{owner}/{name}:{tag}` |
| Git | `git@git.khost.dev:{owner}/{repo}` |
| API | `https://dev.kloudlite.io/v1` |
