# Skeleton Audit — Implementation Plan

**Goal:** every `loading.tsx` holds the exact shape of the page that replaces it, verified by eye
on the rendered page — not inferred from JSX, which is how the 26 Aug rebuild was done and why
some still jump.

**Rule being enforced:** a skeleton earns its place only by matching the layout that replaces it.
One that draws a different layout makes the page jump twice — once to the skeleton, once to the
page — which is worse than a bare spinner.

## Method

Per page, in the real browser (Chrome DevTools MCP against `localhost:3000`, signed in, so auth
and data are real):

1. Throttle the network (`Slow 3G`) and navigate. Screenshot the skeleton while it holds.
2. Un-throttle, let the page land. Screenshot the page.
3. Compare the two on the checklist below. Every miss is a defect to fix in that route's
   `loading.tsx` (or, if the page itself is inconsistent with its siblings, in the page).
4. Fix, reload, re-screenshot. A page is done when the two screenshots overlay with no block
   moving more than one line.

### Checklist, per page

| Check | What "match" means |
|---|---|
| Container | same `<main>` wrapper, same `max-w-page px-6 pt-*` — skeleton inside the layout's container when the layout provides one, drawing its own only when the page does |
| Grid | same grid token at the same breakpoint (`xl:grid-cols-overview`, `xl:grid-cols-code-rail`, `md:grid-cols-settings`, `lg:grid-cols-code`) |
| Heading | present or absent as on the page; same vertical position (the tab row is the heading on owner pages — no extra title block) |
| Toolbar | search / tabs / button row present only where the page has one, same height (`h-8`/`h-9`) |
| First block | the first content block starts at the same y; same border/card treatment |
| Row rhythm | list rows at the same height (`py-3` single-line vs `py-4` two-line) and roughly the same count as a typical page |
| Aside | right rail present only where the page has one, same width token, hidden at the same breakpoint |
| Dark mode | `bg-muted` bones visible on `bg-card` in dark (same class of problem as the landing panel) |

## Pages

35 routes; auth pages are excluded (no data fetch, no suspense). Grouped by the `loading.tsx`
that serves them. Each row records the outcome.

### Root (`(shell)/loading.tsx`)
| Route | Page renders | Result |
|---|---|---|
| `/` signed in | `components/app/home.tsx` — own `<main>`, title, tabs, feed + rail on `overview` | ✔ first block within 1px |

### Own pages under `(shell)`
| Route | Page renders | Result |
|---|---|---|
| `/settings` | `user-settings.tsx` — own `<main>`, title, 7 `SettingsSection`s | ✔ first block within 1px (sections at 218) |
| `/new-repo` | `new-repo-form.tsx` in page's `<main>` | ✔ first block within 1px |
| `/new-team` | `new-team-form.tsx` in page's `<main>` | ✔ first block within 1px (two-line subtitle) |
| `/invite/{token}` | `accept-invite.tsx` — card, no loading yet | skeleton added; card shape |

### Owner pages (`[owner]/loading.tsx` unless listed) — layout provides `<main>`
| Route | Page renders | Result |
|---|---|---|
| `/{owner}` | `(org)/loading.tsx` → `dashboard.tsx`: repo list + activity rail on `overview` | ✔ first block within 1px — was shadowed by the (org) group file |
| `/{owner}/workspaces` | `workspace-list.tsx`: toolbar + list, or empty state | ✔ first block within 1px — rows are 81px two-line |
| `/{owner}/environments` | `environment-list.tsx` | ✔ list shape; page renders empty state locally |
| `/{owner}/snapshots` | `volume-list.tsx` | ✔ first block within 1px |
| `/{owner}/snapshots/{id}` | inline page + `restore-dialog` | ✖ page errors locally (registry client unset); not measured |
| `/{owner}/registries` | `image-list.tsx`: toolbar + list, or empty state with `CopyLine`s | ✔ first block within 1px |
| `/{owner}/registries/{image}` | `registries/[image]/loading.tsx` → title+chip, blurb, `1fr_21rem` grid | ✔ first block within 1px (grid at 212) |
| `/{owner}/registries/{image}/tags` | same loading → title, single card with header + rows | ✔ first block within 1px — own file, card at 186 |
| `/{owner}/registries/{image}/settings` | same loading → `image-settings.tsx` (2 sections) | ✔ first block within 1px — own file, sections at 194 |
| `/{owner}/activity` | `activity/loading.tsx` → `max-w-2xl`, back link, title, feed | ✔ first block within 1px |
| `/{owner}/settings` (team) | `settings/loading.tsx` → `team-settings.tsx` (3 sections) | ✔ first block within 1px — page had a double <main>, fixed |
| `/{owner}/ci` | `NotYet` — title + centred card | ✔ first block within 1px (NotYet: 132 / 186) |

### Repo pages (`[owner]/[repo]/loading.tsx` unless listed) — layout provides `<main pt-6>`
| Route | Page renders | Result |
|---|---|---|
| `/{o}/{r}` | `code.tsx`: toolbar, crumb, listing + README rail on `code-rail` | ✔ within 1px — card at 208 with 45px header, 37px rows |
| `/{o}/{r}/tree/…` | same | ✔ within 1px (same as above) |
| `/{o}/{r}/blob/…` | `file-view.tsx`: toolbar, crumb, file card on `code-rail` | shares the code-view file; not separately measured (fixture had no blob link) |
| `/{o}/{r}/edit/…` | `file-editor.tsx` | shares the code-view file; not measured |
| `/{o}/{r}/commits` | `commits/loading.tsx` → ref picker, day groups | ✔ within 1px — 18px day heading, list at 210, 71px rows |
| `/{o}/{r}/commit/{sha}` | `commit/[sha]/loading.tsx` → back link, card, diffs | ✔ within 5px — 28px back link, card at 164, files line, diffs on mt-3 |
| `/{o}/{r}/pulls` | `pulls/loading.tsx` → title + button, list | ✔ within 1px — 32px title row, list at 180 |
| `/{o}/{r}/pulls/new` | `pulls/new/loading.tsx` → form | ✔ within 1px — 128px base→compare strip |
| `/{o}/{r}/pulls/{n}` | `pulls/[number]/loading.tsx` → header, tabs, conversation + aside on `overview` | ✔ within 1px — header 118px from 164, grid at 305/306 |
| `/{o}/{r}/pulls/{n}/files` | same loading → `pull-files.tsx` on `lg:grid-cols-code` (aside LEFT) | ✔ within 1px — own file on `lg:grid-cols-code`, tree left |
| `/{o}/{r}/pulls/{n}/commits` | same loading → `pull-commits.tsx` | ✔ within 1px — own file, list at 334 |
| `/{o}/{r}/settings` | `settings/loading.tsx` → `repo-settings.tsx` (4 sections) | ✔ within 1px — no subtitle, sections at 186 |
| `/{o}/{r}/actions` | inline page | n/a — the page is a redirect to /{owner}/ci |
| `/{o}/{r}/issues` | `NotYet` | ✔ within 1px (NotYet: 124 / 178) |

## Findings so far (27 Aug)

- Next uses the NEAREST `loading.tsx`: the dashboard skeleton at the `(org)` group level shadowed
  every child route. Each list route has its own file now; `[owner]/loading.tsx` was dead.
- In dev, a client-side navigation without prefetch paints the PARENT segment's skeleton while
  the child is fetched, so measuring via `router.push` is misleading; production `<Link>`s
  prefetch the nested boundary. Skeletons were measured by rendering each `loading.tsx` through a
  temporary route instead (deleted before commit), pages by loading them for real.
- Titles are 30px, subtitles 20px on `mt-1`; owner list rows 81px; the home tab row 45px.
- Bones were `bg-muted` on a white card — invisible in light. Now `bg-border`, with a
  muted-foreground wash in dark.
- Team settings drew its own `<main>` inside the layout's: fixed on the page.
- Repo pages were measured on production against `karthik1729/patch-check` (commits, a merged
  and an open pull); the local API's read path could not list repos. `skeleton-audit` was
  created as a fixture and can be deleted.
- Repo list rows are 37px (`py-2`), commit rows 71px (`py-3.5`), pull rows 81px; the pull
  header is 118px (25px title, 24px state row, 37px tabs) and its body grid lands at 305.

## Known suspects going in

Things the 26 Aug rebuild guessed at and never saw:

- **Empty states.** `workspace-list`, `environment-list`, `volume-list`, `image-list`, `pulls`
  all render a centred `py-14` empty card when there is nothing — a list skeleton then lands on
  a card of a different height. Decide per page: skeleton the list (the state that matters once
  there is data) or the empty card (what a new user sees). Likely: list.
- **Pull files** uses `lg:grid-cols-code` with the file tree on the LEFT; the shared pull
  skeleton draws `overview` with the aside on the right. Needs its own `loading.tsx`.
- **`NotYet` pages** (`/ci`, `/issues`) render a title and a centred card; the owner list
  skeleton draws a toolbar they do not have.
- **`/invite/{token}`** has no skeleton at all; falls through to the home shape with its own
  `<main>` — double container.
- **`/{owner}/snapshots/{id}`** and **`/{o}/{r}/actions`** never had their shape read.
- **Dark mode** on every skeleton — `bg-muted` on `bg-card` was checked in light only.

## Order

Highest traffic first, so a stop halfway still improved the pages people see most:

1. `/`, `/{owner}`, `/{owner}/workspaces`, `/{o}/{r}` — the four landing surfaces
2. `/settings`, `/{owner}/settings`, `/{o}/{r}/settings`
3. the rest of repo, then the rest of owner, then forms and `NotYet`

## Deliverable

Each fixed page: its two screenshots in the PR description, before/after. The table above filled
in. One commit per group in the order above, so a bad one is one revert.
