# Git Repositories

Kloudlite hosts the code. A repository lives in Kloudlite alongside the
[workspaces](workspaces.md) that check it out and the
[environments](environments.md) that run it, with pull requests, issues, and CI/CD
in the same place.

## What is in it

- **Repositories** — hosted git, cloned into workspaces.
- **Issues** — the work items tasks are picked up from.
- **Pull requests** — review and merge of the branches workspaces push.
- **CI/CD** — pipelines triggered by what lands in the repository.

## Why it is here

Follow the refund bug through the loop. It starts as an issue. An agent clones an
ephemeral workspace to work on it. The fix is pushed to a branch and becomes a pull
request. You review it. It merges, CI builds an image, and the image is what runs
in every environment from then on.

With the repository elsewhere, each of those handoffs is a seam: a token so the
workspace can push, a webhook so CI hears about the merge, a dashboard to check
whether the pipeline ran. With the repository in Kloudlite, the platform already
knows that this workspace exists for that issue and will push to that branch. The
seams are not faster; they are gone.

For an agent the difference is larger still. It does not spend its budget
discovering which branch to push to or polling whether the pull request opened —
the same platform that gave it the workspace holds the answer.

<!-- Open: branch and PR semantics for ephemeral workspaces; whether a CI run gets
     its own environment clone; import or mirroring of external repositories. -->
