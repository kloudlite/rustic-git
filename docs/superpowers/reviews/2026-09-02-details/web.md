# Review: web/apps/web

## Findings

**Medium — XSS surface — `src/components/repo/code-block.tsx:12`**
`dangerouslySetInnerHTML={{ __html: html }}` renders shiki's `codeToHtml` output directly. Shiki does not sanitize source text embedded in the code (it escapes text nodes, so this is low risk in practice), but the transformer in `src/lib/highlight.ts:113-118` injects `node.properties.id`/`data-line` from a trusted numeric `line`, not user input, so no injection there either. Flagged because it's the only `dangerouslySetInnerHTML` in the app and worth a comment noting shiki is trusted to escape — no fix needed, verify shiki version stays pinned since escaping is an implicit contract with the dependency.

**Low — informational, not a bug — rounded corner usage vs `--radius: 0`**
`src/components/ui/tooltip.tsx:51`, `src/components/ui/scroll-area.tsx:24,52`, `src/app/(shell)/[owner]/(org)/environments/[id]/snapshots/loading.tsx:11,23`, `src/components/app/env-snapshots.tsx:427` all use `rounded-full`/`rounded-[2px]`/`rounded-[inherit]`. These are all dots, pills, scrollbar thumbs and tooltip arrows, not boxes/cards, so they read as a deliberate exception to the sharp-corner rule rather than a violation. No fix needed.

## Not found (checked, clean)

- Server actions: every `owner`/`id` FormData field that reaches `revalidatePath` is passed through `safeSegment` (`src/lib/slug.ts`), which matches the server's own `valid_segment` rule. Actual authorization is delegated to the api server via the caller's bearer token (`tokenOr()`), never decided client-side — consistent with the CLAUDE.md note that "asking the api IS the check."
- Open redirect: `src/app/(auth)/login/destination.ts` (`safeNext`) rejects `//` and `/\` prefixes before any `redirect()`, and every `?next=`/`redirectTo` in the app funnels through it.
- Tokens: `src/lib/api.ts` is `"server-only"`; no `NEXT_PUBLIC_` env var carries a secret (only a doc comment referencing the convention in `src/lib/clone.ts:4`).
- Fetch timeouts: the shared `call<T>()` in `lib/api.ts` wraps every request in `AbortSignal.timeout(TIMEOUT_MS)` (5s, 15s for slow git ops), with `cache: "no-store"` — no unbounded fetch found.
- `call<T>` usage is centralized to `src/lib/api.ts`; call sites elsewhere just invoke the typed wrapper functions it exports — no `.json()` assumed on error responses or ignored `ok:false` branches spotted in the pages reviewed.
- No sequential-await waterfalls found in the page components inspected (`(org)/page.tsx` uses `Promise.all` for independent reads, matching the CLAUDE.md house-style comments already documenting this).
- `workspace-list.tsx`, `repo-list.tsx`, `image-list.tsx`, `environment-list.tsx` looked like a duplication smell by name but hold materially different data/actions — not copy-paste.

## Scope note

This is a partial-depth review (~15 files read in full out of 234 TS/TSX files) given the effort budget; the codebase is unusually self-documenting (comments explicitly call out the exact bug classes this review was asked to hunt — safeSegment, safeNext, timeouts, Promise.all rationale), which is why so few issues surfaced. A full pass would additionally read: every file under `src/app/(shell)/[owner]/[repo]/**` (PR/merge UI), `src/lib/browse.ts`, and all `useActionState` client forms for race conditions on rapid double-submit.

## Architecture notes

- Auth model: server holds no DB/signing key client-side; all API calls go through `lib/api.ts`, `server-only`, bearer-token or one-time peer-secret at sign-in.
- Route params are trusted for building UI but re-validated (`safeSegment`) before touching `revalidatePath`, since that API takes a pattern, not a literal path.
- Membership/authorization is intentionally never decided in the Next.js layer — a 404 from the api server for "not a member" is treated as the authoritative answer, avoiding a second source of truth for permissions.
- List components differ per entity type by design (different actions/dialogs), not accidental duplication.
- `--radius: 0` is applied consistently to structural elements; rounded utility classes are reserved for dots/pills/scroll thumbs.
# web/apps/web — second-pass review (repo browse, PR/merge UI, lib/api, forms, auth)

Read-only. Every claim below was checked in the file; guesses were dropped.

## Findings

### 0. Every snapshot timestamp is `Invalid Date` — the wire says `createdAt`, the app reads `created_at` — High, correctness (TS/Rust mismatch)
`GET /v1/volumes/{name}/history` builds its rows by hand and emits **camelCase**
`"createdAt"` (`crates/workspaces/src/api.rs:2027`, inside `commit_model_history_rows`); it no
longer serializes `registry::CommitRecord`, whose field is `created_at`
(`crates/workspaces/src/registry.rs:39`). The TS type still declares
`created_at: string` (`src/lib/api.ts:936`) and every reader uses that name:
`src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx:82`,
`.../workspaces/[id]/snapshots/page.tsx:61`,
`.../environments/[id]/snapshots/page.tsx:25`, and
`src/components/app/env-snapshots.tsx:18,128,247,349,362,436,437,487`.
So `new Date(undefined)` → `Invalid Date` on every snapshot row, and
`Date.parse(undefined)` → `NaN` at env-snapshots.tsx:349 and :362 — the restore cutoff
("which record did we move on to") and the snapshot tree's ordering both degrade silently
rather than erroring.
**Fix:** rename the TS field to `createdAt` (`api.ts:936`) and update the eight readers;
the same handler also emits `phase`, which the type omits, and hardcodes `region: ""` /
`state: null` (api.rs:2019-2021), matching the `lineage: never[]` note already in the type.

### 1. `?ref=<sha>` silently shows the wrong history on /commits — Medium, correctness
`src/components/repo/commits.tsx:39` resolves the ref by hand
(`all.value.find(r => shortRef(r.name) === refName) || fallback`) instead of using
`resolveRef` (`src/lib/browse.ts:174`), which is what every other browse page uses and which
also accepts a bare 40-hex oid. So a URL carrying a commit id — produced by the app itself:
`src/components/repo/pull-commits.tsx:74` links `${base}?ref=${c.oid}`, and
`src/components/repo/file-view.tsx:106` carries whatever `?ref=` is current into
`${base}/commits${q}` — falls back to the default branch and lists a history that is not the
one the reader asked for, with the RefPicker showing the default branch as if that were the
request. Nothing errors; the page just answers a different question.
**Fix:** `const head = resolveRef(all.value, refName) ?? fallback` in commits.tsx, and pass
`head.oid` to `log` as it already does. One import, one line; kills the duplicated resolution.

### 2. `..` survives the browse path escaper — Medium, security (defence-in-depth)
`src/lib/browse.ts:63` — `filePath` escapes each segment with `encodeURIComponent`, but `.`
and `..` are unreserved characters and pass through unchanged. The segments come from the
catch-all route params (`blob/[...path]/page.tsx:12`, `tree/[...path]/page.tsx:11`,
`edit/[...path]/page.tsx:14` → `path.join("/")`), so a percent-encoded `%2e%2e` segment that
Next decodes into `..` reaches `fetch(`${BASE}/api/{owner}/{repo}/blob/{oid}/../../…`)`, and
the WHATWG URL parser normalises it away *before* the request goes out — with enough segments
the request lands on a different `/api/{owner}/{repo}/…` endpoint than the page's own.
Not an authorization bypass: the same bearer token is sent and the api tier re-checks
visibility on every read (the comment at `browse.ts:44` is accurate). But the file's own
contract ("every segment is escaped") does not hold, and the guard is one line.
**Fix:** in `filePath`, drop or reject segments equal to `.`/`..`:
`p.split("/").filter(s => s && s !== "." && s !== "..").map(seg).join("/")`.

### 3. Environment `id` reaches `revalidatePath` unvalidated — Low, security hygiene
`src/app/(shell)/[owner]/(org)/environments/actions.ts:61,114,115,173` build
`` `/${owner}/environments/${id}/snapshots` `` from raw FormData. Every sibling action in the
same file validates `owner` with `safeSegment` and the file's header comment (lines 6–10)
states exactly why: a segment carrying `/` or `..` silently revalidates something else. `id`
is held to no rule at all. Impact is confined to cache invalidation (the api call itself is
authorized), but it is the documented invariant broken in the one place it is not applied.
**Fix:** `const id = safeSegment(String(formData.get("id") ?? "")); if (!id) return { error: … }`
— it is a k8s object name, so it already satisfies `safeSegment`.

### 4. `StateBadge` throws on an unrecognised pull state — Low, correctness
`src/components/repo/pull-state.tsx:8-12` indexes a three-key object with `state` and
immediately reads `look.cls`; a fourth state on the wire (`draft`, say) is `undefined` and
takes down the whole pulls list and PR header, not just the badge. The rest of the app is
careful about this (`activity-feed.tsx:43` uses `ICON[e.kind] ?? GitCommitHorizontal`).
**Fix:** `const look = MAP[state] ?? MAP.open;` — same shape as the feed's fallback.

### 5. README fallback costs an extra serial round trip — Low, performance
`src/components/repo/code.tsx:123-152`: `README.md` is fetched speculatively inside the
`Promise.all`, but any other spelling (`readme`, `Readme.md`, `README`) is fetched with a
second `await` *after* the tree has landed, serialised behind it. The speculative fetch also
always costs a request on directories with no README. Cheap and deliberate (the comment says
so), listed only because it is the one genuine waterfall left in the repo pages — `FileView`,
`pullData` and `DiffView` are all correctly parallel or genuinely dependent.
**Fix (optional):** none needed; if it ever matters, ask the api for the directory's README by
name in the `files`/`tree` answer rather than guessing.

### 6. Unbounded lists — Low, performance (all already marked)
- `src/lib/api.ts:614` `listPulls` — flat `?limit=100`, no paging, marked `ponytail:`.
- `src/lib/repo-rail.ts:23` `RAIL_PATH_CAP = 2000` — every repo/tree page ships up to 2000
  paths into the RSC payload for `FileSearch` (`code.tsx:138,167`); marked `ponytail:`.
- PR files/commits tabs render the whole comparison (`pull-files.tsx`, `pull-commits.tsx`);
  the api's own diff ceiling is what bounds them, and `diff.truncated` is surfaced.
No action recommended — the ceilings are named and the upgrade paths are written down.

### 7. Dead export — Low, redundancy
`src/lib/languages.ts:79` `isIgnoredDir` has no reader anywhere in `src` (verified: the only
occurrence in any `.ts`/`.tsx` is its own definition). Delete it.
A further ~20 exports are used only inside their own file (`lib/browse.ts` `logPath`, `Head`,
`WalkedFile`, `CommitDetail`; `lib/api.ts` `ApiUser`, `IssuedToken`, `FileChange`, `Committed`,
`ApiMount`, `ApiRegion`, `TeamProfileInput`, `ApiIssuedInvite`, `ApiPendingCode`;
`lib/env-page.ts` `provenanceOf`, `EnvPage`; `components/app/nav-tabs.tsx` `NavTab`;
`components/app/view-as.tsx` `hrefFor`). Harmless for types; the two functions
(`provenanceOf`, `hrefFor`) could drop `export`.

### 8. Smaller TS-vs-Rust drift — Low, correctness
Verified against the Rust serializers; none of these break at runtime today:
- `ApiEvent.kind` (`src/lib/api.ts:553`) is missing **`"pull_closed"`**, which the feed does
  emit (`crates/api/src/feed.rs:39`). `activity-feed.tsx:43` falls back to the commit icon, so
  a closed pull renders with the wrong glyph rather than crashing. Add the member.
- `ApiComment.at` (`api.ts:578`) is typed `number | { $date: unknown }`, but serialization is
  always the plain number (`crates/pulls/src/pulls/model.rs:143-147` — the bson-date tolerance
  is deserialize-only). The `commentedAt` branch at `pull-conversation.tsx:27` is dead code.
- `ApiComparison` omits the wire's `unknown: bool` (`crates/git/src/browse.rs:464`), so the
  client cannot tell "budget ran out" from "unrelated histories" when `merge_base` is null.
- `ApiVolumeSummary.kind` is narrowed to `"workspace" | "environment"` (`api.ts:901`) but the
  server sends `""` when the parent is gone (`crates/workspaces/src/api.rs:1803`) — the
  archived case the field exists for; `display_name` has the same `unwrap_or_default()` at
  api.rs:1804 and yields `""`, not the volume id its doc comment promises.
Everything else checked matched exactly: `ApiPull`/`ApiMergeability`/`ApiMergeJob` (camelCase
and optionality per `crates/pulls/src/pulls/model.rs:27-89`), `ApiWorkspace`/`ApiEnvironment`/
`WsState`/`EnvState` (`crates/workspaces/src/model.rs`), `ApiRepo.createdAt` (millis,
`crates/api/src/repos.rs:10-21,48`), `ApiTeamDetail` (`crates/api/src/teams.rs:200-234`),
`ApiEvent.at` (seconds, `feed.rs:135`).

## Checked and clean (no finding)

- **Markdown / README rendering** — `code.tsx:64-73`: `react-markdown` with `skipHtml`, GFM
  only, no `dangerouslySetInnerHTML`; `href` and `img src` go through react-markdown's default
  `urlTransform`, so `javascript:`/`data:` are dropped. The one
  `dangerouslySetInnerHTML` in the app (`components/repo/code-block.tsx:12`) receives shiki
  output built from `codeToHtml`, which escapes its input — not user HTML.
- **PR body and comments** are rendered as plain text (`pull-conversation.tsx:20`,
  `whitespace-pre-line`), never as markdown — no injection surface, and deliberately so.
- **External link** `team-profile.tsx:80` is gated by `safeWebsite` (`lib/website.ts:6`, scheme
  allow-list) on both save (`(org)/settings/actions.ts:159`) and render.
- **redirect / open redirect** — every `redirectTo` passes `safeNext`
  (`login/destination.ts:7`, used at `(auth)/actions.ts:16`, `login/actions.ts:43,67`), which
  rejects `//host` and `/\host`.
- **revalidatePath** — every repo/team/image action validates its segments through
  `safeSegment`/`safeRepoPath` (`lib/slug.ts`); pull actions additionally validate `number`
  (`pulls/actions.ts:40`). Only finding 3 is outside that.
- **Token handling** — `apiToken` is read from the encrypted JWT (`lib/api-token.ts:21`) and
  is deliberately not copied onto the session (`auth.ts:192-202`). No client component receives
  it: grepped every `"use client"` file; the only `token` props are an invite token
  (`accept-invite.tsx:10`) and a just-minted PAT shown once (`new-token-dialog.tsx:44`).
- **Double submit / lost state** — every `useActionState` form disables its submit on `pending`
  (or, in `delete-form.tsx:44`, an inert `fieldset`); refused submissions carry `values` back as
  `defaultValue` (`pulls/actions.ts:15`, `settings/actions.ts:15`). No optimistic updates
  anywhere, so there is nothing to roll back.
- **`ok:false` handling** — nothing swallows a failure into `[]` except `listOrSignIn`
  (`lib/require-api.ts:14`), which redirects on `unauthorized` first, and the two documented
  degrade-to-empty reads (`repoRail`, `lastChanges`).
- **Pagination** — `commits.tsx:46-49` pages by cursor with a +1 probe; correct at both ends.
- **Diff parser** (`lib/diff.ts`) — content lines always carry a `+`/`-`/space prefix, so a file
  containing `--- x` cannot be mistaken for a file header; hunk counters reset per `@@`.
- **Merge UI** (`pull-actions.tsx:56-111`) — the strategy is *derived* each render rather than
  held in state, so a retracted fast-forward cannot be submitted; the server re-checks anyway.
- **Heavy client bundles** — shiki, react-markdown and remark-gfm are imported only into server
  components (`lib/highlight.ts` is `server-only`); cmdk/radix search loads via
  `next/dynamic` on first ⌘K (`global-search.tsx:12`).

## Coverage

≈63 of 228 non-test `.ts`/`.tsx` files read in full — the whole requested set: every file under
`app/(shell)/[owner]/[repo]/**`, `lib/browse.ts`, `lib/highlight.ts`, `lib/api.ts` end to end,
`lib/{slug,utils,session,api-token,require-api,diff,time,fuzzy,clone,website,repo-rail}.ts`,
all of `components/repo/*`, the `components/app/*` chrome + form components, `auth.ts` and the
auth actions, plus the workspace/environment/registry server actions and `app/api/*`. Not read:
marketing, `components/ui/*` primitives, onboarding, and most `(org)` page bodies.

## Architecture notes

- The tier split is real and enforced: `lib/api.ts` (`/v1`, the directory) and `lib/browse.ts`
  (`/api`, the git fleet's oid-keyed views) are separate modules, both `server-only`, both
  `cache: "no-store"` with an `AbortSignal.timeout` budget — no Next data cache anywhere, for a
  stated reason (a cached tree outlived a public→private flip).
- Authorization is never decided in the web tier. Guards ask the api and render its 404
  (`guard.ts:28-34`); `lib/session.ts:19-24` says so explicitly and the code honours it.
- Client-controlled strings are funnelled through three small validators — `safeSegment`,
  `safeNext`, `safeWebsite` — each with a comment naming the bug that created it. Findings 2 and
  3 are the two places that funnel has a gap.
- Server components do the fetching; client components are forms and menus only. `useActionState`
  + hidden inputs is the single form idiom, `DeleteForm` the single destructive one.
- Rendering never trusts remote HTML: markdown is parsed with `skipHtml`, code is highlighted
  server-side into escaped spans, PR prose is plain text.
- Deliberate ceilings (pull list of 100, 2000-path rail, 60 s restore poll) are all marked
  `ponytail:` with their upgrade path, which made this pass mostly a check that the markers
  match the code. They do.
