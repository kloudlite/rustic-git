---
name: repos
description: Use when code must be cloned, committed, pushed, or a pull request opened/merged
---

# Repositories

Code lives in a repository. You do not edit it through the platform.

## From the bench

You have no hands here. Cloning, committing, pushing and building are asks to a workspace:
`ask {to: "<workspace>", task: <the person's words>}`. Your own verbs are the listing, creating and
pull-request ones only: `kl_repos`, `kl_repo_create`, `kl_repo_branches`, `kl_pulls`, `kl_pull`,
`kl_pull_create`, `kl_pull_merge`, `kl_pull_close`.

## In a workspace

You clone it into this machine, work there with your own shell and editor tools, and open a pull
request when it is done.

Verbs: `kl_repos`, `kl_repo_create`, `kl_repo_branches`, `kl_repo_clone` (into this machine, over
ssh, with the person's own key), `kl_pulls`, `kl_pull`, `kl_pull_create`, `kl_pull_merge`,
`kl_pull_close`.

Branching, committing and pushing are plain git in your own shell — `bash` — because that is what
they are. Only the pull request itself goes through a tool.

Example:

    kl_repo_clone {repo: "kloudlite/rustic-git"}
    bash {command: "cd rustic-git && git checkout -b fix-login && ... && git push -u origin fix-login"}
    kl_pull_create {repo: "kloudlite/rustic-git", title: "Fix the login redirect", head: "fix-login", base: "master"}
