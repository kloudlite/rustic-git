# Workspace state API (`/fs/*`) — design

**Date:** 2026-09-12 · **Crate:** `crates/ide` · **Status:** approved in chat, building

## Why

The tool API (`/tools/*`) is what an agent drives. A console or IDE that RENDERS a workspace needs
something the agent tools do not give: the file tree as the person sees it, the git state that
colours it, a file's content and its diff. `glob` answers a flat, gitignore-filtered list and
`read` answers numbered text for an agent; neither serves a UI. These routes are separate from the
tools on purpose — they are not in the tool registry, `GET /tools` does not list them, and a
session layer proxies them as they are. Read-only, same confinement, same tunnel, port 7788.

## Serving rules (every route)

- Paths relative to the workspace directory or absolute under the home; anything else is
  `403 {"error"}` through `paths::confine`.
- git is the `git` binary, `current_dir(root)`, bounded to 10 s. Not a repository is not an error:
  `repo: false` and every git field empty.
- One git process per request, never per entry. The tree's letters and ignored flags come from one
  `git status --porcelain=v2 -z --ignored=matching` over the requested level(s).
- Conditional requests: every answer carries an `ETag`; `If-None-Match` answers `304` with no body.
  For a file it is `"{size}-{mtime_ns}"` (no read); for tree/git/changes it is a hash of the body.
- Caps: `depth` 1..3; 5 000 tree entries (`truncated: true`); file 10 MiB (413 past it); a diff
  200 000 bytes (truncated, `truncated: true`).
- JSON everywhere except `/fs/file`, which streams bytes.

## Routes

### `GET /fs/tree?path=.&depth=1`

One directory level by default — the tree is LAZY, the UI expands on click.

```json
{ "path": "/home/kl/workspaces/api", "truncated": false, "entries": [
  { "name": "src", "kind": "dir", "size": 0, "mtime": 1789150000, "ignored": false, "git": "M", "entries": [ … ] },
  { "name": "Cargo.toml", "kind": "file", "size": 812, "mtime": 1789150000, "ignored": false, "git": "M" },
  { "name": ".cache", "kind": "dir", "size": 0, "mtime": 1789150000, "ignored": true, "git": "" },
  { "name": "link", "kind": "symlink", "size": 0, "mtime": 1789150000, "ignored": false, "git": "", "target": "src/lib.rs" }
] }
```

Directories first, then names, byte order. Hidden entries INCLUDED; `ignored` tells a UI what to
grey or hide (`.git` is always `ignored: true`). `git` is one letter: the worktree column when set,
else the index column; `?` untracked; `""` clean. A directory carries `M` when anything below it is
dirty, `?` when everything below it is untracked. Nested `entries` only when `depth > 1`.

### `GET /fs/stat?path=`

One tree row plus `mime` for a file. 404 when nothing is there. The cheap refresh of one node after
the watch stream reported it.

### `GET /fs/file?path=&at=`

The bytes. `Content-Type` sniffed (text/*; charset=utf-8 for text, the detected type otherwise,
`application/octet-stream` when unknown), `Content-Length`, `ETag`, `Last-Modified`. `at=HEAD` (or
any ref, or `index`) answers the committed version through `git show {at}:{path}` — the other side
of a diff view. 404 when absent at that ref. Binary files are served as they are; the UI decides
how to show them. 413 past 10 MiB.

### `GET /fs/git`

```json
{ "repo": true, "branch": "main", "head": "2261c195…", "upstream": "origin/main",
  "ahead": 1, "behind": 0, "dirty": true, "stashes": 0 }
```

`git status --porcelain=v2 --branch -z` headers plus `git stash list` count. Detached ⇒
`branch: null`; no upstream ⇒ `upstream: null`, `ahead`/`behind` 0. Not a repo ⇒ `{ "repo": false }`.

### `GET /fs/changes`

The changes panel in one call: status AND counts.

```json
{ "repo": true, "changes": [
  { "path": "crates/ide/src/fs.rs", "index": "A", "worktree": ".", "renamed_from": null,
    "additions": 210, "deletions": 0, "binary": false }
] }
```

Status from porcelain v2; `additions`/`deletions` from one `git diff --numstat HEAD` (a `-` pair
means `binary: true`; an untracked file counts its lines). Order: git's.

### `GET /fs/diff?path=&against=`

One file's unified diff, `against` = `HEAD` (default: worktree vs HEAD, what the person changed since
the last commit), `index` (worktree vs index) or `staged` (index vs HEAD). Untracked files diff
against `/dev/null` so a new file renders as all additions. Answer:

```json
{ "path": "…", "against": "HEAD", "binary": false, "truncated": false, "patch": "diff --git …" }
```

Plain unified text; the UI parses hunks the way the PR page already does (`pull-files.tsx`). No
`path` ⇒ the whole tree's patch, same cap.

## Live updates

Nothing new. `watch` on `.` plus `GET /stream/watch/{id}` names the changed path; the UI re-fetches
that node (`/fs/stat`), its parent (`/fs/tree`) and `/fs/changes`. Every GET is conditional, so a
storm of re-fetches on unchanged data is 304s.

## Not in this cut

Writes (the tool API has `write`/`edit`; the session layer decides who may), git WRITES (commit,
checkout, stage — same reason), a recursive full-tree dump (lazy depth), search (the `grep` tool),
gzip (the tunnel is ssh; enable at the session layer if it leaves the machine).

## Tests

`crates/ide/src/fs.rs` unit tests against a tempdir with a real `git init` and one commit: porcelain
v2 parsing (branch headers, modified, untracked, rename, detached), tree order and ignored flag from
a `.gitignore`, the dir letter roll-up, symlink target, depth bound, numstat join, a diff of a
modified and of an untracked file, `at=HEAD` content, ETag/304, not-a-repo answers. `server.rs`:
one route test per path for the status codes (200, 304, 400 depth, 403 outside, 404 stat/file, 413).
