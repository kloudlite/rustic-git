# Git

Repositories live at `git.khost.dev` under your handle or a team's. A workspace seeded from a repository has it cloned into the tree; from then on it is ordinary git.

## Remote URL

```
git@git.khost.dev:{owner}/{repo}
```

The ssh key on your account authenticates. Inside a workspace your platform key is present, so `git push` works without setup. Any other host (GitHub, GitLab) works the same way if its key is in the workspace or forwarded with `ssh -A`.

## Seeding a workspace

`repo` and `branch` on create clone into the tree on first start. The clone runs inside the pod with your platform key; nothing is stored for it.

```json
{ "name": "api", "region": "centralindia-k3s", "quota_gb": 20, "repo": "git@git.khost.dev:acme/api", "branch": "main" }
```

## Pull requests

Open and merge pull requests in the console under the repository. Merges run server-side with the real `git` binary: merge, squash, or rebase, refused when the branch is not mergeable.

## Snapshots are not commits

A [push](../snapshots/push.md) snapshots the whole tree, including untracked and ignored files. Commit and push code with git; push a snapshot when you want the whole working state back later.
