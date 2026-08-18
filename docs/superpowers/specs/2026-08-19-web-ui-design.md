# The web UI: a browser for repositories, and eventually a platform

The read API exists and is deployed. This is the interface on top of it: a Next.js
application that lets a person browse a repository in a browser, built so that
workspaces, environments, registries and pipelines can join it later without a
redesign.

Read-only first. Nothing here creates, deletes or pushes.

## The model the interface must tell the truth about

Git's own model decides most of the layout, and getting it wrong is what made the
first four attempts confusing.

- A **commit** is an immutable snapshot plus parent pointers, identified by its SHA.
  It belongs to no branch.
- A **branch** is a name pointing at one commit. In this backend it is literally a
  key whose value is an oid — `list_refs` returns `(String, ObjectId)`.
- A **tag** is the same mechanism with a different lifetime expectation.
- "Commits on main" means *reachable from whatever main points at now*, never ownership.

From which the central rule follows:

**Every view is either a question asked of a moving ref, or a fixed object.**

| | Moving | Fixed |
|---|---|---|
| Pages | tree, file, commit list | a commit and its diff |
| Ref control | a branch picker — the ref is a parameter of the question | none; a `Fixed at <sha> · reachable from …` chip |
| Link stability | changes when the branch moves; offers a Permalink to pin it | permanent |

This is the same distinction the storage layer already makes: every endpoint is keyed
by `{oid}`, `/refs` exists to resolve a name to an oid once, and the cache treats
id-addressed answers as immutable while `/refs` gets five seconds.

## Navigation: three tiers, each fact stated once

1. **Platform** — org switcher, and which resource type (Repositories · Workspaces ·
   Environments · Registries · Pipelines).
2. **Repository** — which repo, and which section (Code · Pull requests · Reviews ·
   Issues · Settings).
3. **Workbench** — a full-width 48px toolbar: ref control on the left, the path
   *inside the repo* in the middle, the section sub-nav on the right.

The breadcrumb carries **only** the path within the repo. Organisation, section, repo
and tab are already on screen above; repeating them is what made the earlier
breadcrumbs unreadable.

Going back has exactly one control per journey. Walking a path up is the breadcrumb.
Leaving a detail page for the list it came from is a single named control at the top
of the content — "Back to commits" — never a bare arrow, and never alongside a
second mechanism doing the same job.

## Each view brings its own navigator

Consistency belongs in the shell, not in showing panels that do not apply. Showing an
irrelevant one is worse than showing nothing: it implies a relationship that is not there.

| View | Navigator |
|---|---|
| Files, File | the repository file tree |
| Commits | none — the list is the navigator, with scope and filters beside it |
| Diff | the files changed in this commit |

Actions on an object live with the object: Code/Blame/Raw and Permalink sit in the
file card's header, not in the section toolbar.

## Pages

| Route | Shows |
|---|---|
| `/{owner}` | Repositories — language, pipeline health, last activity per row |
| `/{owner}/{repo}` | Code · Files at the repo root, with README and repo status |
| `/{owner}/{repo}/tree/{ref}/{path}` | Code · Files at a path |
| `/{owner}/{repo}/blob/{ref}/{path}` | Code · File |
| `/{owner}/{repo}/commits/{ref}` | Code · Commits |
| `/{owner}/{repo}/commit/{oid}` | Code · Diff |

`{ref}` is a branch name for humans; the server component resolves it to an oid once
via `/refs`, and every call after that is oid-keyed and cacheable forever — mirroring
the backend's own caching design.

## Design language

Open Sans throughout. Distinctiveness comes from structure, not from a display face.

- **Brand** teal `#0E7C66` — links, active states, SHAs, folder icons, primary action.
  Deliberately not the blue-or-black every developer tool defaults to.
- **Neutrals** `#101828` ink · `#475467` body · `#98A2B3` label · `#EAECF0` line ·
  `#F9FAFB` ground · `#FFFFFF` surface.
- **Semantic** `#17B26A` pass · `#F04438` fail · `#B54708` warn — status only, never decoration.
- **Type** 24 / 20 / 15 / 14 / 13 / 11. Monospace for every identifier: SHA, path, size, code.
- **Space** 4 / 8 / 12 / 16 / 20 / 24 / 32. Cards at 6px radius, 1px hairline,
  `0 1px 2px rgba(16,24,40,.05)`.

Width is a decision, not a default. Content pages (repositories, and later the other
tier-1 sections) sit on a constrained centred column, because a four-row table stretched
across a metre of screen reads as empty. Workbench pages run full width, because a tree
plus code is a tool and should use the space.

## Themes

Light and dark, both first class. Every colour is a CSS custom property; no component
hard-codes a hex. The dark palette is a deliberate re-mapping rather than an inversion —
teal lightens to hold contrast on a dark ground, and the semantic hues shift with it.
Theme follows the system by default, with an explicit override persisted for the user.

## Responsive

Three breakpoints, and the layout degrades by dropping panels in a fixed order rather
than by reflowing everything.

| Width | Behaviour |
|---|---|
| ≥1280 | Full layout: navigator, content, context column |
| 768–1279 | Context column collapses into the content, below it |
| <768 | Navigator becomes a drawer behind a button; toolbar wraps to two rows; the file table drops its "last commit" column, keeping name and age |

The diff keeps its changed-files list at every width — it *is* the navigation for that
page, so it becomes a horizontal strip rather than disappearing.

## Architecture

Turborepo with bun.

```
apps/web        Next.js: pages as server components, server actions, theming
packages/api    typed client for the read API — refs/tree/blob/log/commit
packages/ui     shadcn components and the token layer
packages/config tsconfig, eslint, tailwind preset
```

**No route handlers.** The browser gets server-rendered HTML and server actions, and
nothing else. `packages/api` is `server-only`, so an accidental client import fails at
build. The browser has no endpoint to enumerate, no token to steal, no request shape to
forge — a user can only do what a rendered page or a server action allows.

A raw-file download and a machine-readable API are deliberate non-goals for this
version; adding either is a decision to take on its own merits, not something that
leaks in by default.

## Errors

The backend collapses "private" and "missing" to 404 by design, and the web app must
not be smarter than that. A signed-in user hitting a repo they cannot see gets the same
404 page as a repo that does not exist. Turning that into "this repo is private, sign
in?" would leak existence — the exact property the backend spent three review rounds
protecting.
