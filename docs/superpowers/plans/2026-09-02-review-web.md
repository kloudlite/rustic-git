# Web App Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the six actionable web findings of the 2026-09-02 review: every snapshot timestamp
rendering `Invalid Date` because the wire says `createdAt` and the app reads `created_at`; `?ref=<sha>`
on /commits silently listing the default branch's history; `..` surviving the browse path escaper;
an unvalidated environment `id` reaching `revalidatePath`; `StateBadge` throwing on an unknown pull
state; and one dead export. Plus a one-line comment recording that shiki's escaping is a dependency
contract.

**Architecture:** No structural change. Five of the six are one-line guards or renames at the point
the value is read. The one new module is `src/lib/snapshot.ts` — a single pure reader for a snapshot
record's timestamp, so the wire field name lives in exactly one place instead of the eight it lives
in today, and so the fix has something a `bun test` can fail on. Everything else reuses helpers that
already exist and are already tested (`resolveRef`, `safeSegment`).

**Tech Stack:** Next.js app router, TypeScript, Tailwind, bun (`cd web && bun run typecheck / lint / test`)

**Spec:** docs/superpowers/reviews/2026-09-02-codebase-review.md (details: docs/superpowers/reviews/2026-09-02-details/web.md)

## Global Constraints
- Tokens over raw Tailwind colours; `--radius: 0` — no task here touches styling, so nothing new to round.
- `src/lib/api.ts` and `src/lib/browse.ts` are `server-only`: any test importing them must `mock.module("server-only", () => ({}))` first, as `src/lib/browse.test.ts:5` does.
- `*.test.ts` files are excluded from `tsc`, so `bun run typecheck` will not check them — run `bun test` too.
- Comments explain WHY, never what; keep any `ponytail:` marker you edit near.
- Commit subjects are imperative sentence case, no attribution trailers of any kind.
- Do not widen scope: finding 8 of the detail review (further TS-vs-Rust drift — `pull_closed`, `ApiComment.at`, `ApiComparison.unknown`, `ApiVolumeSummary.kind`) is outside this plan.

---

### Task 1: Read the snapshot timestamp the API actually sends

**Files:**
- Create `web/apps/web/src/lib/snapshot.ts`
- Create `web/apps/web/src/lib/snapshot.test.ts`
- Modify `web/apps/web/src/lib/api.ts:936` (the `ApiCommitRecord.created_at` field)
- Modify `web/apps/web/src/components/app/env-snapshots.tsx:18,128,247,349,362,436,437,487`
- Modify `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx:82`
- Modify `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/snapshots/page.tsx:25`
- Modify `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/[id]/snapshots/page.tsx:61`

**Interfaces:**
- Consumes: `GET /v1/volumes/{name}/history`, whose rows are built by hand in
  `crates/workspaces/src/api.rs:2012-2033` (`commit_model_history_rows`) and emit
  `"createdAt": sn.creation_timestamp().map(|t| t.0.to_string())` at api.rs:2027 — an RFC3339
  string, or `null` for an object with no creation timestamp. The comment at api.rs:2025 names a
  Rust test `commit_model_history_rows_createdat_is_rfc3339`; **it does not exist in the tree**
  (grep finds only the comment). Do not go write it — it is a Rust-side finding, not this plan's.
- Produces: `snapshotTime(record): number` — milliseconds, or `NaN` when the wire sent `null`.

- [ ] **Step 1: Write the failing test** — create `web/apps/web/src/lib/snapshot.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { snapshotTime } from "./snapshot";

// One row exactly as `commit_model_history_rows` builds it
// (`crates/workspaces/src/api.rs:2012-2033`): camelCase `createdAt`, an RFC3339 string from
// `jiff::Timestamp`'s Display, plus the `phase` the TS type does not declare and the hardcoded
// `region: ""` / `state: null`. Recorded here so a rename on either side fails loudly.
const ROW = {
  id: "snap-4f2c",
  state: null,
  lineage: [],
  region: "",
  message: "before the migration",
  createdAt: "2026-09-02T11:04:07Z",
  parent: null,
  phase: "Ready",
};

describe("snapshotTime", () => {
  test("reads the wire's camelCase field", () => {
    expect(snapshotTime(ROW)).toBe(Date.parse("2026-09-02T11:04:07Z"));
    expect(Number.isFinite(snapshotTime(ROW))).toBe(true);
  });

  test("a row that never got a creation timestamp is NaN, not the epoch", () => {
    // `creation_timestamp()` is an Option (api.rs:2027). NaN renders as "Invalid Date", which is
    // honest; 1970 would be a lie the tree would happily sort on.
    expect(snapshotTime({ ...ROW, createdAt: null })).toBeNaN();
  });

  test("the old snake_case name is not what the server sends", () => {
    // The bug this file exists for: reading `created_at` off this row gave `undefined`, and
    // `new Date(undefined)` is Invalid Date on every snapshot row in the app.
    expect((ROW as Record<string, unknown>).created_at).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/snapshot.test.ts`.
  Expected failure: `error: Cannot find module './snapshot' from '.../src/lib/snapshot.test.ts'`.

- [ ] **Step 3: Implement**

Create `web/apps/web/src/lib/snapshot.ts`:

```ts
/** When a snapshot record was taken, in millis.
 *
 *  The one place the wire's field name lives. `/v1/volumes/{name}/history` builds its rows by
 *  hand (`crates/workspaces/src/api.rs:commit_model_history_rows`) and emits camelCase
 *  `createdAt`; it used to serialize `registry::CommitRecord`, whose field is `created_at`, and
 *  eight readers here kept the old name — every one of them silently produced `Invalid Date`.
 *  `NaN` for a record with no timestamp: an unorderable row is the truth, the epoch is not. */
export function snapshotTime(record: { createdAt: string | null }): number {
  return record.createdAt ? Date.parse(record.createdAt) : NaN;
}
```

`web/apps/web/src/lib/api.ts:936` — inside `ApiCommitRecord`, replace

```ts
  created_at: string;
```

with

```ts
  /** RFC3339. camelCase because `/history` builds its rows by hand rather than serializing
   *  `CommitRecord` (`crates/workspaces/src/api.rs:2027`); `null` when the object carries no
   *  creation timestamp. Read it through `snapshotTime` (`lib/snapshot.ts`), never by hand. */
  createdAt: string | null;
```

`web/apps/web/src/components/app/env-snapshots.tsx` — add the import beside the existing
`stamp, when` import (line 12):

```ts
import { snapshotTime } from "@/lib/snapshot";
```

line 18:

```ts
export type SnapshotNode = { id: string; message?: string; createdAt: string | null; parent: string | null };
```

line 128:

```ts
    ? `“${current.message || "snapshot"}” (${when(snapshotTime(current))})`
```

line 247:

```ts
              Delete snapshot &ldquo;{label}&rdquo; ({when(snapshotTime(snapshot))})? The
```

line 349:

```ts
          : (history.find((h) => snapshotTime(h) > since && descends(h, restored.id)) ?? restored);
```

line 362:

```ts
        return onPath(a) - onPath(b) || snapshotTime(a) - snapshotTime(b);
```

lines 436-437:

```tsx
                      <span title={stamp(snapshotTime(current))}>
                        {when(snapshotTime(current))}
```

line 487:

```ts
          const ts = new Date(snapshotTime(c));
```

`web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx:82` — import
`snapshotTime` from `@/lib/snapshot` and replace

```tsx
                  {when(new Date(at.created_at).getTime())}
```

with

```tsx
                  {when(snapshotTime(at))}
```

`web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/snapshots/page.tsx:25`:

```tsx
      history={history.map((c) => ({ id: c.id, message: c.message, createdAt: c.createdAt, parent: c.parent ?? null }))}
```

`web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/[id]/snapshots/page.tsx:61` — import
`snapshotTime` from `@/lib/snapshot` and replace

```tsx
                  {when(new Date(c.created_at).getTime())}
```

with

```tsx
                  {when(snapshotTime(c))}
```

Then `cd web && grep -rn "created_at" apps/web/src` must return nothing.

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src && git commit -m "Read snapshot timestamps from the wire's createdAt field"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 2: Resolve `?ref=` on /commits the way every other browse page does

**Files:**
- Modify `web/apps/web/src/components/repo/commits.tsx:5` (import) and `:38` (the hand-rolled find)
- Test: `web/apps/web/src/lib/browse.test.ts:33-41` already covers `resolveRef` — no new test file

**Interfaces:**
- Consumes: `resolveRef(all: Ref[], refName?: string): Head | undefined` (`src/lib/browse.ts:174-182`),
  which accepts a short branch/tag name, a bare 40-hex oid, and falls back to the default branch.
- Produces: a `head` whose `.oid` feeds `log` and whose `.name` feeds `RefPicker current`. `Head`
  is `{ name; oid; kind }`, structurally what the current code's `Ref` gives it — `shortRef(head.name)`
  on a commit head returns the oid unchanged, which is what the picker should show.

- [ ] **Step 1: Write the failing test** — none, and deliberately: the defect is in an async server
  component (`CommitsView`) that fetches on render, and the helper it should have called is already
  covered by `apps/web/src/lib/browse.test.ts:33-41` ("a short name, an oid, or the default"),
  including the bare-oid case that is the whole bug. Standing up an RSC harness to re-assert a
  tested one-liner is more machinery than the fix. Verify by hand instead, in Step 3.

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/browse.test.ts`;
  expect it to PASS unchanged (the helper is correct; only its caller was not using it). The
  failure this task fixes is observed by hand, not by bun: open
  `/{owner}/{repo}/commits?ref=<40-hex oid>` — today the RefPicker reads the default branch and
  the list is the default branch's history, not the requested commit's.

- [ ] **Step 3: Implement**

`web/apps/web/src/components/repo/commits.tsx:5`:

```ts
import { log, refs, resolveRef, shortOid, shortRef } from "@/lib/browse";
```

(`defaultBranch` stays imported — `fallback` is still used for `RefPicker defaultBranch` at line 65.)

`web/apps/web/src/components/repo/commits.tsx:37-38`:

```ts
  const fallback = defaultBranch(all.value);
  // `resolveRef`, not a find by name: `?ref=` here carries a bare oid as often as a branch —
  // pull-commits.tsx:74 and file-view.tsx:106 both produce one — and a hand-rolled name match
  // fell back to the default branch and listed a history nobody asked for.
  const head = resolveRef(all.value, refName);
```

Nothing else in the file changes: `head.oid` already feeds `log` (line 45) and `shortRef(head.name)`
already feeds the picker (line 64). Re-check by hand that `?ref=<oid>` now lists that commit's
first-parent walk and the picker shows the oid.

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src/components/repo/commits.tsx && git commit -m "Resolve the commits page ref with resolveRef so an oid works"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 3: Drop `.` and `..` segments in the browse path escaper

**Files:**
- Modify `web/apps/web/src/lib/browse.ts:63-64` (`filePath`)
- Modify `web/apps/web/src/lib/browse.test.ts` (add a `describe` block; import `filePath`)

**Interfaces:**
- Consumes: catch-all route params joined with `/` — `blob/[...path]/page.tsx:12`,
  `tree/[...path]/page.tsx:11`, `edit/[...path]/page.tsx:14`. Next decodes `%2e%2e` into `..`
  before the page ever sees it.
- Produces: a path whose segments are all escaped and none of which the WHATWG URL parser will
  normalise away. Callers: `tree` (browse.ts:70), `blob` (browse.ts:75) and everything downstream.

- [ ] **Step 1: Write the failing test** — append to `web/apps/web/src/lib/browse.test.ts`, and add
  `filePath` to the destructured import on line 6:

```ts
describe("filePath", () => {
  test("keeps the slashes, escapes the segments", () => {
    expect(filePath("src/lib/a b.ts")).toBe("src/lib/a%20b.ts");
    expect(filePath("")).toBe("");
  });

  test("drops dot segments", () => {
    // `.` and `..` are unreserved, so encodeURIComponent passes them through untouched and the
    // URL parser then normalises them away before the request leaves — landing the fetch on a
    // different /api endpoint than the page's own. The api tier re-checks visibility either way,
    // so this is the file's own contract holding, not an authorization fix.
    expect(filePath("a/../../b")).toBe("a/b");
    expect(filePath("./a/./b")).toBe("a/b");
    expect(filePath("..")).toBe("");
    // A file whose NAME merely starts with dots is not a dot segment and must survive.
    expect(filePath("...hidden/..x")).toBe("...hidden/..x");
  });
});
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/browse.test.ts`.
  Expected failure: first `SyntaxError`/`undefined is not a function` on the missing `filePath`
  export, and once exported, `expect(filePath("a/../../b")).toBe("a/b")` fails with
  `Expected: "a/b"  Received: "a/../../b"`.

- [ ] **Step 3: Implement** — `web/apps/web/src/lib/browse.ts:63-64`:

```ts
const seg = (s: string) => encodeURIComponent(s);
/** A path keeps its slashes — it is many segments — but every segment is escaped.
 *
 *  `.` and `..` are dropped rather than escaped: they are unreserved characters, so
 *  `encodeURIComponent` leaves them alone and the URL parser resolves them away before the
 *  request goes out, which walks the fetch off this repo's own `/api/{owner}/{repo}/…` prefix.
 *  Git has no such path anyway, so dropping is exact, not lossy. */
export const filePath = (p: string) =>
  p.split("/").filter((s) => s && s !== "." && s !== "..").map(seg).join("/");
```

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src/lib/browse.ts web/apps/web/src/lib/browse.test.ts && git commit -m "Drop dot segments in the browse path escaper"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 4: Validate the environment `id` before it becomes a revalidate pattern

**Files:**
- Modify `web/apps/web/src/app/(shell)/[owner]/(org)/environments/actions.ts` — the four actions
  whose `id` reaches `revalidatePath`: `pushEnvironment` (`:52-64`, path built at `:61`),
  `restoreEnvironmentFrom` (`:108-116`, paths at `:114-115`), `deleteEnvironmentSnapshot`
  (`:162-175`, path at `:173`). Read the whole file first: `startEnvironment` (`:21`),
  `stopEnvironment`, `cloneEnvironment` (`:119`) and `deleteEnvironmentSnapshots` (`:178`) all read
  the same `id` field, and any of them that reaches a `revalidatePath` gets the same guard.
- Modify `web/apps/web/src/lib/slug.test.ts` (add the k8s-name cases the guard relies on)

**Interfaces:**
- Consumes: `safeSegment(s: string): string | null` (`src/lib/slug.ts:8-11`) — ASCII letters,
  digits, `-`, `_`, `.`, 1–100 chars, never `.`/`..` alone. A k8s object name is a subset of that,
  so no real submission is refused.
- Produces: `{ error }` before any api call when `id` is not a segment; unchanged behaviour otherwise.

- [ ] **Step 1: Write the failing test** — the guard is `safeSegment`, already exported and pure;
  the actions themselves are `"use server"` and need `next/cache` + a token to run, which is a
  harness this fix does not earn. Pin the property the fix depends on instead — append to
  `web/apps/web/src/lib/slug.test.ts`, inside the existing `describe("safeSegment", …)`:

```ts
  test("accepts every k8s object name, rejects what would move a revalidate", () => {
    // Environment ids are k8s object names (RFC 1123 subdomain), which is a strict subset of the
    // rule above — so guarding `id` with safeSegment refuses no real submission.
    for (const id of ["env-4f2c", "web.staging", "e", "0"]) expect(safeSegment(id)).toBe(id);
    for (const id of ["", "..", "env/../other", "env%2f..", "a b"]) expect(safeSegment(id)).toBeNull();
  });
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/slug.test.ts`.
  Expect it to PASS: `safeSegment` is already correct, and this test is the contract the guard
  leans on. The finding is that four call sites never call it; verify that first with
  `cd web && grep -n 'formData.get("id")' 'apps/web/src/app/(shell)/[owner]/(org)/environments/actions.ts'`
  — every hit is a bare `String(...)` today, which is the failure this task removes.

- [ ] **Step 3: Implement** — in `web/apps/web/src/app/(shell)/[owner]/(org)/environments/actions.ts`,
  replace every

```ts
  const id = String(formData.get("id") ?? "");
```

with

```ts
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };
```

  in each action that goes on to call `revalidatePath` with it — `pushEnvironment` (line 55),
  `restoreEnvironmentFrom` (the `id` read above line 108), `cloneEnvironment` (line 122),
  `deleteEnvironmentSnapshot` (line 165), `deleteEnvironmentSnapshots`, and `startEnvironment`
  (line 24) / `stopEnvironment` if their revalidate paths embed `id` too. `safeSegment` is already
  imported at line 10; extend that import's comment (lines 6-10) so the reason covers both fields:

```ts
// `owner` and `id` reach every action below as FormData, and go straight into a revalidatePath
// PATTERN. A segment carrying `/` or `..` would silently revalidate something else, so each
// action refuses it — a bad one is never a real submission, since the pages that render these
// forms fill the field from the route params.
```

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src && git commit -m "Validate the environment id before revalidating a path with it"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 5: Give `StateBadge` a fallback instead of a crash

**Files:**
- Modify `web/apps/web/src/components/repo/pull-state.tsx:8-12`
- Create `web/apps/web/src/components/repo/pull-state.test.ts`

**Interfaces:**
- Consumes: `PullState` (`src/lib/api.ts:606`, `"open" | "merged" | "closed"`) — but the value
  arrives from the api server, so a fourth state on the wire is a runtime possibility the type
  cannot prevent.
- Produces: a badge; never `undefined.cls`. Same fallback shape as `activity-feed.tsx:43`
  (`ICON[e.kind] ?? GitCommitHorizontal`).

- [ ] **Step 1: Write the failing test** — create `web/apps/web/src/components/repo/pull-state.test.ts`.
  Testing the lookup, not the JSX: the component is a plain function over a constant map, so the
  map is what has to not throw. Export it for that.

```ts
import { describe, expect, test } from "bun:test";
import { LOOK } from "./pull-state";

describe("StateBadge's lookup", () => {
  test("every declared state has its own word and icon", () => {
    expect(LOOK.open.label).toBe("Open");
    expect(LOOK.merged.label).toBe("Merged");
    expect(LOOK.closed.label).toBe("Closed");
  });

  test("an unknown state off the wire falls back rather than taking the list down", () => {
    // The badge is rendered inside the pulls list and the PR header; indexing the map with a
    // state it does not hold (`draft`, say) used to return undefined and `undefined.cls` threw
    // the whole page, not just the badge.
    const look = LOOK[("draft" as unknown as keyof typeof LOOK)] ?? LOOK.open;
    expect(look.cls).toBe(LOOK.open.cls);
  });
});
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/components/repo/pull-state.test.ts`.
  Expected failure: `SyntaxError: Export named 'LOOK' not found in module '.../pull-state.tsx'`.

- [ ] **Step 3: Implement** — `web/apps/web/src/components/repo/pull-state.tsx`, replacing lines 7-14:

```tsx
/** Exported for the test: the fallback below is the whole point of this map, and asserting it
 *  through rendered JSX would need a DOM for one property lookup. */
export const LOOK = {
  open: { Icon: CircleDot, label: "Open", cls: "border-success/40 bg-success/10 text-success" },
  merged: { Icon: GitMerge, label: "Merged", cls: "border-primary/40 bg-primary/10 text-primary" },
  closed: { Icon: GitPullRequestClosed, label: "Closed", cls: "border-destructive/40 bg-destructive/10 text-destructive" },
};

export function StateBadge({ state, className }: { state: PullState; className?: string }) {
  // A state the wire grows and this build has not heard of (`draft`) is `undefined` here, and
  // the badge sits inside the pulls list and the PR header — a throw takes both down over a
  // pill. Same fallback the activity feed uses for an unknown event kind.
  const look = LOOK[state] ?? LOOK.open;
```

  (the returned JSX at lines 14-19 is unchanged.)

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src/components/repo/pull-state.tsx web/apps/web/src/components/repo/pull-state.test.ts && git commit -m "Fall back to the open badge on an unknown pull state"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

### Task 6: Delete `isIgnoredDir`, and record shiki's escaping contract

**Files:**
- Modify `web/apps/web/src/lib/languages.ts:75-81` (delete `IGNORED_DIRS` and `isIgnoredDir`)
- Modify `web/apps/web/src/lib/languages.test.ts:2,17-20` (drop the import and the test of it)
- Modify `web/apps/web/src/components/repo/code-block.tsx:10-12` (one comment, no behaviour change)

**Interfaces:**
- Consumes: nothing. `grep -rn "isIgnoredDir" web/apps/web/src` returns three hits — its own
  definition (`languages.ts:79`) and its own test (`languages.test.ts:2,18-19`). `IGNORED_DIRS`
  (`languages.ts:75`) has no other reader either; the directory filtering the rail does is
  server-side.
- Produces: nothing. The two other "used only in their own file" exports the review names
  (`provenanceOf`, `hrefFor`) are **kept exported** — both have tests that import them
  (`view-as.test.ts:2`), so dropping `export` would break the suite. Leave them.

- [ ] **Step 1: Write the failing test** — none to add; the failing check for a deletion is the
  deletion itself. Record the current state first so Step 3 is verifiable:
  `cd web && grep -rn "isIgnoredDir\|IGNORED_DIRS" apps/web/src` — expect exactly five hits
  (definition, set, and three in the test file).

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test apps/web/src/lib/languages.test.ts`.
  Expect it to PASS today (the dead function has a passing test — that is the finding: a test is
  its only caller). After Step 3 the same command must pass with the block gone; if it errors with
  `Export named 'isIgnoredDir' not found`, the test edit was missed.

- [ ] **Step 3: Implement**

`web/apps/web/src/lib/languages.ts` — delete lines 75-81 entirely:

```ts
const IGNORED_DIRS = new Set([
  "node_modules", "vendor", "dist", "build", "target", ".git", "third_party",
]);

export function isIgnoredDir(name: string) {
  return IGNORED_DIRS.has(name);
}
```

`web/apps/web/src/lib/languages.test.ts:2` becomes:

```ts
import { breakdown, languageOf } from "./languages";
```

and lines 17-20 (the `test("build output is skipped as a directory", …)` block, with its trailing
blank line) are deleted.

`web/apps/web/src/components/repo/code-block.tsx` — extend the existing doc comment (lines 3-7) with
one line, changing nothing else:

```tsx
/** A highlighted source block. Async server component: shiki runs once per render
 *  on the server and the browser receives coloured spans, nothing to hydrate —
 *  including the scrollbar, which is plain CSS overflow rather than a mounted
 *  ScrollArea per block.
 *
 *  The app's only `dangerouslySetInnerHTML`: safe because `codeToHtml` escapes the source it
 *  wraps, which is a contract with the dependency rather than something checked here — keep
 *  shiki pinned (`web/apps/web/package.json`) and re-read its escaping on a major bump. */
```

- [ ] **Step 4: `cd web && bun run typecheck && bun run lint && bun test`**
- [ ] **Step 5: Commit** — `git add web/apps/web/src && git commit -m "Delete the unused isIgnoredDir helper"`; NO Co-Authored-By, NO Claude-Session, NO attribution trailers.

---

## Self-review

| Finding (details/web.md) | Where it lands |
|---|---|
| Summary High #8 / detail #0 — `created_at` vs `createdAt`, 8 readers | Task 1 |
| #1 Medium — `?ref=<sha>` resolved by hand in `commits.tsx` | Task 2 |
| #2 Medium — `..` survives `filePath` in `browse.ts` | Task 3 |
| #3 Low — environment `id` reaches `revalidatePath` unvalidated | Task 4 |
| #4 Low — `StateBadge` throws on an unknown pull state | Task 5 |
| #5 Low — README fallback costs a serial round trip | **deferred:** the review's own fix is an API change — the `/api/{owner}/{repo}/tree` answer would have to name the directory's README so the web tier stops guessing. Nothing worth doing on this side alone: dropping the speculative `README.md` fetch trades one wasted request for a guaranteed second round trip, which is worse. |
| #6 Low — unbounded lists (`listPulls` limit 100, `RAIL_PATH_CAP` 2000, PR files/commits) | **deferred:** all three need an API change to fix — a `?page=` on `/pulls` (marked at `api.ts:630`), a `/languages/{oid}` answer plus server-side file search (marked at `repo-rail.ts:21-24`), and the api's own diff ceiling for the PR tabs. Every ceiling is already a `ponytail:` marker naming its upgrade path, and the review recommends no action. |
| #7 Low — dead export `isIgnoredDir` | Task 6 (the other ~20 single-file exports are left alone: the two functions the review suggests un-exporting, `provenanceOf` and `hrefFor`, are imported by their own tests) |
| Detail-pass Medium — shiki `dangerouslySetInnerHTML` | Task 6, comment only — the review asks for a note, not a fix |
| #8 Low — further TS-vs-Rust drift (`pull_closed`, `ApiComment.at`, `ApiComparison.unknown`, `ApiVolumeSummary.kind`) | **deferred:** outside the scope this plan was given, and none of it breaks at runtime today. |
| First-pass Low — `rounded-full` / `rounded-[2px]` vs `--radius: 0` | **deferred:** the review itself concludes no fix is needed — dots, pills and scrollbar thumbs are the deliberate exception. |
