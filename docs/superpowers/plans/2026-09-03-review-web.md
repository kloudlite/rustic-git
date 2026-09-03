# Web review remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every finding of the 2026-09-03 web review in `web/apps/web` — four Important, six Minor, seven Cleanup — plus dropping `ApiWorkspace.live_state`, without adding a dependency or a new UI pattern.

**Architecture:** Decisions move into `web/apps/web/src/lib/*.ts` as pure functions with `bun:test` coverage (the idiom `ws-status.ts`, `snapshot-state.ts`, `archived.ts` already set); components keep only rendering, and their correctness is proven by `tsc` + `lint` rather than by a DOM test harness (there is no component test runner in this repo — do not add one). Nothing here touches the Rust tiers; the `live_state` removal is the web half of a change the workspaces-api plan makes on the server.

**Tech Stack:** Next.js app router (see `web/apps/web/AGENTS.md` — the installed Next.js differs from training data, read `node_modules/next/dist/docs/` before touching routing), React 19 server components + server actions, TypeScript, Tailwind with design tokens, `bun test` (`bun:test`), Turborepo.

**Spec:** `docs/superpowers/reviews/2026-09-03-details/web.md` (summarised in `docs/superpowers/reviews/2026-09-03-codebase-review.md`). Read the whole detail report before Task 1 — its "Good, leave alone" section names things that must NOT change.

## Global Constraints

- **Vocabulary in user-facing copy:** workspace, environment, push, snapshot, restore, clone, delete. Sync points are internal and **never shown**. "Lineage" is not in the vocabulary (comments may keep it; screens may not).
- **No new dependency.** Not one, in any task. If a task looks like it needs one, it is the wrong task.
- **Design tokens over raw Tailwind colours** — no `text-red-500` and friends; use `text-destructive`, `text-warning`, `text-muted-foreground`, `border-border`.
- **`--radius: 0`** — no `rounded-*` class anywhere in `src` (the one existing `rounded-full` on the 6px live dot in `env-snapshots.tsx` is pre-existing; do not add more).
- **Copy existing siblings, not new patterns.** `repo-list.tsx` for filterable lists, repo `settings/` for destructive actions, `lib/time.ts` for size/date formatting, `lib/archived.ts`/`lib/ws-status.ts` for pure decision modules.
- **Commit messages:** imperative sentence case, no tool attribution — no "Co-Authored-By", no "Generated with", no Claude/session trailer, on any commit in this plan.
- **Gates, run from `web/`, after every task:**
  ```sh
  cd web && bun run lint                              # expect exit 0, "1 successful, 0 warnings"
  bunx tsc --noEmit -p apps/web/tsconfig.json         # expect exit 0, no output
  bun test                                            # expect exit 0, 0 fail
  ```
  A non-zero exit from any of the three means the task is not done. Baseline at the start of this plan is lint clean, tsc clean, 105 pass / 0 fail across 25 files.
- Editor TS diagnostics in this app are frequently stale; trust the `bunx tsc` run, nothing else.

## Decisions taken before Task 1 (do not re-litigate)

- **`react-markdown` + `remark-gfm` stay.** The one consumer is `Markdown` in `components/repo/code.tsx:66-73`, which renders arbitrary third-party READMEs: it needs a CommonMark+GFM *parser* (tables, task lists, autolinks) with a safe `urlTransform` and `skipHtml`, while `shiki` is a syntax highlighter with no markdown AST — replacing them means hand-writing a markdown parser, which is more code and a new XSS surface for the app's only untrusted-HTML-adjacent path. Keep; no task.
- **The three restore dialogs are not merged** (review C4 says so explicitly). Only the two `DeleteSnapshotDialog`s are.
- **No `ws` branch in `place()`** (review M6). A comment, nothing else.

## File Structure

| file | change |
|---|---|
| `web/apps/web/src/app/(shell)/[owner]/(org)/volume-actions.ts` | I1 — return a discriminated result |
| `web/apps/web/src/components/app/archived-snapshots.tsx` | I1 (three states), M4 (dialog id) — also loses its local `DeleteSnapshotDialog` usage? no: keeps its own delete-volume dialog untouched |
| `web/apps/web/src/lib/env-current.ts` (new) + `.test.ts` (new) | I2 — the one `current` computation |
| `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx` | I2 — call it |
| `web/apps/web/src/components/app/env-snapshots.tsx` | I2, M1, M4, M5, C4 |
| `web/apps/web/src/lib/env-page.ts` + `lib/env-page.test.ts` | I3, C6 — delete `Provenance`/`provenanceOf`/the test file |
| `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/page.tsx`, `environments/page.tsx` | I4 — `Promise.all` |
| `web/apps/web/src/components/app/workspace-list.tsx` | M2 (positive Stop test), C5 (move `Notices` out) |
| `web/apps/web/src/lib/packages-field.ts` (new) + `.test.ts` (new), `workspaces/actions.ts`, `lib/api.ts` | M3 |
| `web/apps/web/src/components/app/shell-nav.tsx` | M6 — one sentence in the `place()` doc comment |
| `web/apps/web/src/lib/api.ts`, `lib/snapshot.test.ts` | C1, C2, C3 — drop `live_state`, `lineage`, `region`, `latest_ms` |
| `web/apps/web/src/components/app/restore-dialog.tsx` | C4 — the shared `DeleteSnapshotDialog` lands here |
| `web/apps/web/src/components/app/wsenv-state-badge.tsx`, `environment-list.tsx` | C5 — `Notices` moves here |
| `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/settings/page.tsx` | C7 — one comment |

---

### Task 1: I1 — a failed history read must not read as "no snapshots"

**Files:**
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/volume-actions.ts:14-19`
- Modify: `web/apps/web/src/components/app/archived-snapshots.tsx:41-56, 86-105, 138-140`

**Interfaces:**
- Produces: `volumeSnapshots(name: string): Promise<{ ok: true; rows: ApiCommitRecord[] } | { ok: false; error: string }>` — the only caller is `RestoreDialog` in `archived-snapshots.tsx`.

**Why:** `lib/require-api.ts:7-13` records the incident this repeats — an expired token turned every list into `[]` and made every repo look deleted. Here the same collapse tells someone standing at a deleted workspace that the last copy of their data is gone, on the one screen where that is their actual fear, while `lib/archived.ts:31` only produced the row because `v.snapshots > 0`.

There is no pure function in this change (the action is `"use server"` and reads `tokenOr()` from a `server-only` module; the consumer is a client component). Its proof is `tsc` — the component cannot compile against the new union until all three states are handled — plus the manual check in Step 5. Do not invent a test double for `tokenOr`.

- [ ] **Step 1: Change the action's return type**

Replace `volume-actions.ts:14-19` with:

```ts
export async function volumeSnapshots(
  name: string,
): Promise<{ ok: true; rows: ApiCommitRecord[] } | { ok: false; error: string }> {
  const token = await tokenOr();
  // An expired session is NOT "no snapshots" — see `lib/require-api.ts`. Collapsing either
  // failure into `[]` told someone their last copy was gone, silently.
  if (typeof token !== "string") return { ok: false, error: token.error };
  const r = await api.volumeHistory(token, name);
  return r.ok ? { ok: true, rows: r.value } : { ok: false, error: r.message || "Could not read the snapshots." };
}
```

Keep the existing doc comment on lines 7-13 verbatim above it — it explains why the read is lazy, which is unchanged.

- [ ] **Step 2: Run tsc to see the consumer fail**

Run: `cd web && bunx tsc --noEmit -p apps/web/tsconfig.json`
Expected: FAIL in `components/app/archived-snapshots.tsx` — the `setSnaps(s)` call no longer matches `ApiCommitRecord[] | null`.

- [ ] **Step 3: Give the dialog three states**

In `archived-snapshots.tsx`, change the state and the effect (lines 41-56):

```tsx
  // Three states, not two: not read yet, failed, and genuinely empty. A failed read that renders
  // as "no snapshots" is a false claim of data loss.
  const [snaps, setSnaps] = useState<ApiCommitRecord[] | null>(null);
  const [snapsError, setSnapsError] = useState<string | null>(null);
  const [sel, setSel] = useState("");

  useEffect(() => {
    if (!open || snaps !== null || snapsError !== null) return;
    let live = true;
    volumeSnapshots(row.id).then((r) => {
      if (!live) return;
      if (!r.ok) {
        setSnapsError(r.error);
        return;
      }
      setSnaps(r.rows);
      // Newest first from the api, and the newest is what a restore almost always means.
      setSel(r.rows[0]?.id ?? "");
    });
    return () => {
      live = false;
    };
  }, [open, snaps, snapsError, row.id]);
```

and the picker block (lines 86-105) — insert the failed branch FIRST, so an error never falls through to the empty copy:

```tsx
            {snapsError !== null ? (
              <p role="alert" className="text-sm2 font-medium text-destructive">
                Could not read the snapshots — try again. ({snapsError})
              </p>
            ) : snaps === null ? (
              <p className="text-sm2 text-muted-foreground">Reading the snapshots…</p>
            ) : snaps.length === 0 ? (
              <p role="alert" className="text-sm2 text-muted-foreground">
                No snapshots left to restore from.
              </p>
            ) : (
```

The `select` branch and everything after it is unchanged. The submit button's `disabled={pending || !sel}` (line 138) already keeps Restore unusable in the failed state — leave it. **Do not touch `DeleteVolumeDialog` (`archived-snapshots.tsx:148`)**: its button lives outside this dialog and must stay usable when the list failed, because it is the one action whose meaning does not depend on the list.

- [ ] **Step 4: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0; `bun test` still 105 pass / 0 fail.

- [ ] **Step 5: Check the failure path by hand**

Temporarily make the action's first line `return { ok: false, error: "boom" };`, run `bun run dev`, open a team's Workspaces page → Snapshots → Restore on an archived row, and confirm the dialog shows the destructive "Could not read the snapshots — try again" line and that Delete volume on the same row is still clickable. Revert the temporary line before committing (`git diff` must show only the intended change).

- [ ] **Step 6: Commit**

```bash
git add "web/apps/web/src/app/(shell)/[owner]/(org)/volume-actions.ts" web/apps/web/src/components/app/archived-snapshots.tsx
git commit -m "Show a failed snapshot read as an error, not as no snapshots"
```

---

### Task 2: I2 — one `current` computation, shared by the header and the Snapshots tab

**Files:**
- Create: `web/apps/web/src/lib/env-current.ts`
- Create: `web/apps/web/src/lib/env-current.test.ts`
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx:338-355` (the `restored` / `since` / `current` / `foreignCurrent` block) and its `byId`/`descends` helpers at 332-337
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx:42` (`const at = …`) and its comment at 47-48

**Interfaces:**
- Produces:
  ```ts
  export type CurrentInput = { id: string; createdAt: string | null; parent?: string | null };
  export type EnvCurrent<T> = { current: T | null; foreign: string | null };
  export function envCurrent<T extends CurrentInput>(
    history: T[],
    opts: { live: boolean; restoredTo: string | null; restoredAt: string | null },
  ): EnvCurrent<T>;
  ```
  Generic over the row type so the layout can pass `ApiCommitRecord[]` and the tab `SnapshotNode[]` — both structurally satisfy `CurrentInput`.
- Consumes: `snapshotTime` from `lib/snapshot.ts` (never `Date.parse(record.createdAt)` by hand — that rename cost eight silent `Invalid Date`s once).

**Why:** the layout uses "`restored_to` when set, else `history[0]`" and its comment claims that is what the tab does. It is not: the tab takes the newest record pushed *after* `restore_requested_at` that *descends from* `restored_to`, and renders a foreign `restored_to` honestly instead of falling through to `history[0]`. The header is the more-read surface and it is the wrong one; a wrong "you are here" on a restore screen is how someone restores the wrong point. The fix deletes the second algorithm rather than syncing it.

- [ ] **Step 1: Write the failing test**

Create `web/apps/web/src/lib/env-current.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { envCurrent } from "./env-current";

/** Newest first, exactly as `/v1/volumes/{name}/history` returns them. */
const at = (id: string, iso: string, parent: string | null = null) => ({ id, createdAt: iso, parent });
const c = at("c", "2026-09-03T12:00:00Z", "b");
const b = at("b", "2026-09-02T12:00:00Z", "a");
const a = at("a", "2026-09-01T12:00:00Z", null);
const HISTORY = [c, b, a];

describe("envCurrent", () => {
  test("never restored: the newest record", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: null, restoredAt: null }))
      .toEqual({ current: c, foreign: null });
  });

  test("restored and nothing pushed since: the restored record itself", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: "a", restoredAt: "2026-09-03T18:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });

  test("restored then pushed: the new record on the restored branch, not the old tip", () => {
    const d = at("d", "2026-09-04T12:00:00Z", "a");
    expect(envCurrent([d, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: d, foreign: null });
  });

  test("a newer record on a SIBLING branch is not current", () => {
    // Pushed after the restore, but descends from `b` — the branch the environment left.
    const sib = at("sib", "2026-09-04T12:00:00Z", "b");
    expect(envCurrent([sib, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });

  test("a foreign restored_to names no record: no current, and the id is reported", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: "other-vol-snap", restoredAt: "2026-09-03T18:00:00Z" }))
      .toEqual({ current: null, foreign: "other-vol-snap" });
  });

  test("archived: no live environment sits anywhere", () => {
    expect(envCurrent(HISTORY, { live: false, restoredTo: null, restoredAt: null }))
      .toEqual({ current: null, foreign: null });
  });

  test("no records at all", () => {
    expect(envCurrent([], { live: true, restoredTo: null, restoredAt: null }))
      .toEqual({ current: null, foreign: null });
  });

  test("a record with no timestamp never wins the after-the-restore test", () => {
    const undated = at("undated", null as unknown as string, "a") as { id: string; createdAt: string | null; parent: string | null };
    expect(envCurrent([undated, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });
});
```

- [ ] **Step 2: Run it to see it fail**

Run: `cd web && bun test apps/web/src/lib/env-current.test.ts`
Expected: FAIL — `Cannot find module './env-current'`.

- [ ] **Step 3: Write `lib/env-current.ts`**

```ts
import { snapshotTime } from "@/lib/snapshot";

/** The shape both callers' rows already have: the api's `ApiCommitRecord` and the Snapshots
 *  tab's `SnapshotNode`. Generic so each caller gets its OWN row type back. */
export type CurrentInput = { id: string; createdAt: string | null; parent?: string | null };

export type EnvCurrent<T> = {
  /** The record the environment sits on, or `null` — archived, or a `restoredTo` from elsewhere. */
  current: T | null;
  /** A `restoredTo` naming no record here: a restore grafted ANOTHER volume's snapshot in place.
   *  Badging any record `current` would claim the environment is on a snapshot it is not. */
  foreign: string | null;
};

/** Where an environment sits, decided ONCE for both the header and the Snapshots tab.
 *
 *  Never restored: the newest record (one straight chain). Restored: the newest record pushed
 *  AFTER the restore that descends from the restored one — the environment moved on to it — else
 *  the restored record itself. Its older children are the branches the environment left behind.
 *
 *  This lived twice, with two different answers, and the header's was the wrong one: it fell
 *  through to `history[0]` after an in-place restore and after a foreign one.
 *
 *  `history` is newest first, as `/v1/volumes/{name}/history` returns it. */
export function envCurrent<T extends CurrentInput>(
  history: T[],
  { live, restoredTo, restoredAt }: { live: boolean; restoredTo: string | null; restoredAt: string | null },
): EnvCurrent<T> {
  if (!live) return { current: null, foreign: null };
  if (restoredTo === null) return { current: history[0] ?? null, foreign: null };

  const byId = new Map(history.map((h) => [h.id, h]));
  const restored = byId.get(restoredTo) ?? null;
  if (!restored) return { current: null, foreign: restoredTo };

  const descends = (n: T, anc: string): boolean => {
    for (let p: T | undefined = n; p; p = p.parent ? byId.get(p.parent) : undefined) {
      if (p.id === anc) return true;
    }
    return false;
  };
  // `NaN > since` is false, so an undated record never wins this — an unorderable row is the
  // truth (`lib/snapshot.ts`), and guessing would move the badge onto it.
  const since = restoredAt ? Date.parse(restoredAt) : 0;
  return {
    current: history.find((h) => snapshotTime(h) > since && descends(h, restored.id)) ?? restored,
    foreign: null,
  };
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd web && bun test apps/web/src/lib/env-current.test.ts`
Expected: PASS, 8 tests.

- [ ] **Step 5: Call it from the Snapshots tab**

In `env-snapshots.tsx`, delete the `byId`, `descends`, `restored`, `since`, `current`, `foreignCurrent` block (lines 332-355) and its now-duplicated comments, and put in its place:

```tsx
  // One rule, shared with the environment header — see `lib/env-current.ts`.
  const { current, foreign: foreignCurrent } = envCurrent(history, {
    live: envName !== null,
    restoredTo,
    restoredAt,
  });
```

Add `import { envCurrent } from "@/lib/env-current";` beside the other `@/lib` imports (after the `@/lib/archived` line, alphabetical is not enforced here — match the file's existing grouping). `byId` may be used further down the file for the rail's `childrenOf`; run tsc after the delete and re-add only what is still referenced, keeping it below the `envCurrent` call.

- [ ] **Step 6: Call it from the layout**

In `environments/[id]/layout.tsx`, replace line 42:

```ts
const at = env ? (history.find((c) => c.id === env.restored_to) ?? history[0] ?? null) : null;
```

with:

```ts
// The same call the Snapshots tab makes, so the header and the tab cannot disagree about where
// the environment sits — they did, and the header was the wrong one.
const { current: at } = envCurrent(history, {
  live: env !== null,
  restoredTo: env?.restored_to ?? null,
  restoredAt: env?.restore_requested_at ?? null,
});
```

Add `import { envCurrent } from "@/lib/env-current";`. Replace the stale comment at lines 47-48 ("Same rule the tab uses: `restored_to` when set, else the newest record.") with: `{/* Where the environment sits, from the one shared rule — see lib/env-current.ts. */}`. The `{at && …}` render below is unchanged; with a foreign `restored_to` it now correctly renders nothing rather than asserting a record.

- [ ] **Step 7: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0; test count is now 113 pass / 0 fail (105 + 8).

- [ ] **Step 8: Commit**

```bash
git add web/apps/web/src/lib/env-current.ts web/apps/web/src/lib/env-current.test.ts web/apps/web/src/components/app/env-snapshots.tsx "web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/layout.tsx"
git commit -m "Decide the current snapshot once for the header and the tab"
```

---

### Task 3: I3 + C6 — delete `Provenance`, `provenanceOf` and its test

**Files:**
- Modify: `web/apps/web/src/lib/env-page.ts:5-13, 53, 59` and its `ApiService` import on line 3
- Delete: `web/apps/web/src/lib/env-page.test.ts`

**Why:** `provenanceOf` reads a `name` field `/v1/volumes/{name}/history` has never sent since it stopped serializing `upstream::Provenance` — `snapshot_rows` (`crates/workspaces/src/api.rs:snapshot_rows`) emits `spec.state`, i.e. `crd::SnapshotState`, whose two variants are `{kind, image, packages, resources, quotaGb, attachedEnvironment}` and `{kind, services, quotaGb}`. There is no `name` in either, ever, so `newest.name` is permanently `undefined` and the third fallback in the `name` chain is dead. It compiles only because `provenanceOf` takes `unknown` and casts — the same module already types the field correctly as `SnapshotState` at `lib/api.ts`. `env-page.test.ts` pins a shape the server cannot produce. If a provenance name is genuinely wanted later, the fix is a field in `snapshot_rows`, not a re-cast here.

- [ ] **Step 1: Delete the test file**

```bash
git rm web/apps/web/src/lib/env-page.test.ts
```

- [ ] **Step 2: Run the tests to confirm nothing else depended on it**

Run: `cd web && bun test`
Expected: PASS, 24 files, 4 fewer assertions than the previous task's run (the file was 1 test).

- [ ] **Step 3: Delete the type, the function and the dead fallback**

In `lib/env-page.ts`: delete lines 5-13 (the `Provenance` doc comment, the type and `provenanceOf`), delete line 53 (`const newest = provenanceOf(history[0]?.state);`), and change line 59 to:

```ts
    name: env?.name ?? volume?.display_name ?? id,
```

Then drop `ApiService` from the line-3 import **only if** it is no longer referenced — it still is, by `EnvPage.services`, so keep it. Run tsc to confirm.

- [ ] **Step 4: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0. `tsc` clean proves no other file imported `Provenance` or `provenanceOf`.

- [ ] **Step 5: Commit**

```bash
git add web/apps/web/src/lib/env-page.ts web/apps/web/src/lib/env-page.test.ts
git commit -m "Drop provenanceOf, which reads a shape /history no longer sends"
```

---

### Task 4: I4 — the two list pages fetch in parallel

**Files:**
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/page.tsx:15, 27`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/environments/page.tsx:16, 28`

**Why:** `listVolumes` does not depend on the list — both are keyed only off `token` and `owner` — and each carries `TIMEOUT_MS = 5_000` (`lib/api.ts:20`), so the worst case for these two pages is a 10 s render and the typical case is two serial round trips where one would do. Multiplied by `AutoRefresh`'s 10 s poll (2 s while any row is `creating`), so it is sustained load. The org dashboard and `loadEnvPage` already do this correctly; these two are the outliers and the two busiest pages of the workspaces product.

There is no pure piece here and no way to assert an ordering without a test harness this repo does not have; the proof is `tsc` plus reading the diff. Do not add one.

- [ ] **Step 1: Parallelise the workspaces page**

In `workspaces/page.tsx`, replace line 15 and line 27 with a single `Promise.all` placed where line 15 is, keeping BOTH existing comments above it:

```ts
  // The URL's owner is the team when it is not the person themselves; the api decides
  // membership and answers 404 for a team they are not in.
  //
  // The Snapshots section, the same one the environments page carries and for the same reason: a
  // volume whose Workspace is gone and whose snapshots are not. Deleting a workspace keeps them,
  // so this is the only way back to them — and the only place they can be deleted for good.
  // A failed read leaves the section empty rather than failing the page: the working set above is
  // what someone came here for.
  //
  // Neither read depends on the other; serial, they cost two 5 s timeouts instead of one.
  const scope = owner === session.user.owner ? undefined : owner;
  const [list, volumes] = await Promise.all([
    listWorkspaces(token, scope),
    listVolumes(token, "workspace", scope),
  ]);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }
```

Everything from `const rows = volumes.ok ? volumes.value : [];` down is unchanged — `list` still redirects/404s/throws, `volumes` still degrades to `[]`.

- [ ] **Step 2: Parallelise the environments page**

In `environments/page.tsx`, same shape, keeping the `mine` variable and both comment blocks:

```ts
  const mine = owner === session.user.username;
  const [list, volumes] = await Promise.all([
    listEnvironments(token, mine ? undefined : owner),
    listVolumes(token, "environment", mine ? undefined : owner),
  ]);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }
  const rows = volumes.ok ? volumes.value : [];
```

- [ ] **Step 3: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 4: Confirm the pages still render**

`bun run dev`, open `/{owner}/workspaces` and `/{owner}/environments` — the working list and the Snapshots section both render as before. Note for the reviewer: **do not** add a per-row history fetch; `volume-actions.ts:7-13` deliberately avoids that N+1 and the review calls it out as the good half of this design.

- [ ] **Step 5: Commit**

```bash
git add "web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/page.tsx" "web/apps/web/src/app/(shell)/[owner]/(org)/environments/page.tsx"
git commit -m "Fetch the list and its volumes in parallel on both list pages"
```

---

### Task 5: M1 — "lineage" out of user-facing copy

**Files:**
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx:255, 434, 450` (line numbers from the review; find them by the strings below — earlier tasks shift them)

**Why:** the vocabulary is workspace / environment, push, snapshot, restore, clone, delete. Sync points are never shown. "Lineage" is a fourth word for the snapshot chain, appears only in this one file's copy, and already means something else internally (`ApiCommitRecord.lineage`, always `[]`). Comments may keep the word; screens may not.

- [ ] **Step 1: Rewrite the three strings**

| find | replace |
|---|---|
| `"No snapshots yet — take one to start the lineage"` | `"No snapshots yet — take one to start"` |
| `this lineage &mdash; changes since are not snapshotted` | `this environment&rsquo;s snapshots &mdash; changes since are not snapshotted` |
| `but the lineage will no longer show where it is.` | `but the snapshots will no longer show where it is.` |

The second one's surrounding sentence reads `…is no longer in ` + the replacement; keep the `&mdash;`. Leave the doc comment on the `DeleteSnapshotDialog` (line 221, "the lineage still showing where the environment sits") alone — it is a comment.

- [ ] **Step 2: Prove no user-facing "lineage" is left**

Run: `cd web && grep -rn "lineage" apps/web/src --include="*.tsx"`
Expected: only the line-221 doc comment (and, until Task 12, nothing in `.ts` types matters here). Any hit inside JSX text or a string literal is a failure.

- [ ] **Step 3: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/components/app/env-snapshots.tsx
git commit -m "Say snapshots, not lineage, in the environment copy"
```

---

### Task 6: M2 — Stop only for a workspace that is running

**Files:**
- Modify: `web/apps/web/src/components/app/workspace-list.tsx:372`

**Why:** `running={w.state !== "stopped"}` gives the Stop button to `creating` (nothing to stop yet), `error` and `deleted`. The environment side gets this right with a positive test (`env-actions.tsx:210` — `running={state === "running"}`), and this file's own line 359 says *"A button that only ever errors is worse than no button"*.

- [ ] **Step 1: Make the test positive**

```tsx
                {/* Positive test, as the environment's ToggleForm uses: `creating`, `error` and
                    `deleted` have nothing to stop, and a button that only ever errors is worse
                    than no button. */}
                <ToggleForm owner={owner} id={w.id} running={w.state === "ready"} />
```

`ready` is the workspace's running state (`WsState`; the environment's equivalent is `running` — see the `LOOK` map in `wsenv-state-badge.tsx`, which covers both). A `stopped` workspace still gets Start, unchanged.

- [ ] **Step 2: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 3: Check it by eye**

`bun run dev`, open `/{owner}/workspaces`: a `ready` row shows Stop, a `stopped` row shows Start, a `creating` row shows Start (nothing to stop). No row shows a Stop that would error.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/components/app/workspace-list.tsx
git commit -m "Offer Stop only for a running workspace"
```

---

### Task 7: M3 — one explicit rule for whether `packages` goes on the wire

**Files:**
- Create: `web/apps/web/src/lib/packages-field.ts`
- Create: `web/apps/web/src/lib/packages-field.test.ts`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts:72-74`
- Modify: `web/apps/web/src/lib/api.ts` — `restoreWorkspace`'s `...(extra?.packages ? …)` spread (around line 876)

**Why:** the action decides by `formData.has("packages")` and `api.restoreWorkspace` decides again by truthiness. Today they agree by luck (`[]` is truthy), which is the right answer — the snapshot had no packages and the user saw and accepted that. But the comment above the api call says the opposite intent ("an omitted one must stay off the wire entirely"), so one of the two guards is doing nothing, and "fixing" the truthiness read to `extra?.packages?.length` would silently make a package-less restore inherit the snapshot's own list.

**Interfaces:**
- Produces: `packagesField(fd: FormData): string[] | undefined` — `undefined` when the field is absent (let the api use the snapshot's own list), a possibly-empty array when it is present.

- [ ] **Step 1: Write the failing test**

Create `web/apps/web/src/lib/packages-field.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { packagesField } from "./packages-field";

const fd = (v?: string) => {
  const f = new FormData();
  if (v !== undefined) f.set("packages", v);
  return f;
};

describe("packagesField", () => {
  test("an absent field is undefined — the snapshot's own list stands", () => {
    expect(packagesField(fd())).toBeUndefined();
  });

  test("a present but empty field is an EMPTY list, not undefined", () => {
    // The snapshot froze `packages: []`, the input rendered blank, and the person accepted it.
    // Sending nothing would silently restore the snapshot's list instead.
    expect(packagesField(fd(""))).toEqual([]);
  });

  test("a list is split, trimmed, and blanks dropped", () => {
    expect(packagesField(fd(" ripgrep ,, fd,"))).toEqual(["ripgrep", "fd"]);
  });
});
```

- [ ] **Step 2: Run it to see it fail**

Run: `cd web && bun test apps/web/src/lib/packages-field.test.ts`
Expected: FAIL — `Cannot find module './packages-field'`.

- [ ] **Step 3: Write `lib/packages-field.ts`**

```ts
/** The restore form's `packages` field, as the api wants it.
 *
 *  Presence is the whole rule, and it is decided HERE, once: an absent field means "use the
 *  definition the snapshot froze", and a present-but-blank field means "the snapshot had none,
 *  and that is what I accepted". Those two were being decided twice — by `has()` in the action
 *  and by truthiness in `restoreWorkspace` — and a tidy-up of either would have made a
 *  package-less restore silently inherit a list the person never saw. */
export function packagesField(fd: FormData): string[] | undefined {
  if (!fd.has("packages")) return undefined;
  return String(fd.get("packages")).split(",").map((p) => p.trim()).filter(Boolean);
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd web && bun test apps/web/src/lib/packages-field.test.ts`
Expected: PASS, 3 tests.

- [ ] **Step 5: Use it, and make the api's guard match**

In `workspaces/actions.ts`, replace lines 72-74 with:

```ts
  const packages = packagesField(formData);
```

keeping the comment above them, and add `import { packagesField } from "@/lib/packages-field";`.

In `lib/api.ts`'s `restoreWorkspace` body, change the spread from truthiness to presence:

```ts
    ...(extra?.packages !== undefined ? { packages: extra.packages } : {}),
```

The comment above the `extra` parameter already says exactly this ("an omitted one must stay off the wire entirely") — leave it; the code now matches it.

- [ ] **Step 6: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0; 3 more tests than the previous task's run.

- [ ] **Step 7: Commit**

```bash
git add web/apps/web/src/lib/packages-field.ts web/apps/web/src/lib/packages-field.test.ts "web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts" web/apps/web/src/lib/api.ts
git commit -m "Decide the packages field's presence in one place"
```

---

### Task 8: M4 — the restore dialog's label id is per-snapshot

**Files:**
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx:172-178` (the `htmlFor="restore-keep-message"` label and the `id="restore-keep-message"` `Input`)

**Why:** `RestoreDialog` is rendered once per history row, each with the same DOM id. It is safe only because Radix unmounts a closed `DialogContent` — a property of the dialog library, not of this code. `forceMount` or a portal change turns it into duplicate ids, and the label then points at whichever comes first. `archived-snapshots.tsx:85` already does this correctly with `` id={`snap-${row.id}`} ``.

- [ ] **Step 1: Interpolate the snapshot id**

The component already has `snapshot` in scope (it renders `label` from `snapshot.message || snapshot.id.slice(0, 8)`). Use it:

```tsx
                    <label htmlFor={`restore-keep-${snapshot.id}`} className="text-muted-foreground">
```
```tsx
                      id={`restore-keep-${snapshot.id}`}
```

Both must change; a mismatched pair is worse than the shared id. Follow the sibling's idiom (`snap-${row.id}`) rather than `useId()` — no new import, and the id stays readable in the DOM.

- [ ] **Step 2: Prove no hardcoded per-row id is left**

Run: `cd web && grep -rn "restore-keep-message" apps/web/src`
Expected: no output (exit 1).

- [ ] **Step 3: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/components/app/env-snapshots.tsx
git commit -m "Key the restore dialog's label id by snapshot"
```

---

### Task 9: M5 — announce the push that lands, not only the one that times out

**Files:**
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx` — the pending node's `uploading…` span (around line 481) and the `current` badge (around line 510)

**Why:** the pending row renders `uploading…` as plain text. A screen-reader user who submitted the push hears nothing when the row appears, nothing when it lands, and then — five minutes later — a `role="alert"` interrupting them. The transition that matters is silent; the rare one is loud. The `role="alert"` on the stale message is correct and stays.

- [ ] **Step 1: Make the pending status a polite live region**

```tsx
                  <span
                    role="status"
                    className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground"
                  >
                    uploading…
                  </span>
```

- [ ] **Step 2: Announce the landing**

The pending node unmounts when the record lands, and an unmount announces nothing — so the badge that appears in its place carries the announcement:

```tsx
                    <span
                      role="status"
                      className="border border-primary/40 bg-primary/10 px-1.5 py-0.5 text-caption font-medium text-primary"
                    >
                      current
                    </span>
```

That is the `isCurrent` badge inside the record row. It is present on every render of the current row, not only after a push, so it announces on first paint too — acceptable and the cheapest honest option; do not add a keyed visually-hidden line and a piece of state to make it fire only after a push.

- [ ] **Step 3: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/components/app/env-snapshots.tsx
git commit -m "Announce a push landing to a screen reader"
```

---

### Task 10: M6 — say why a workspace's snapshots page stays `org`

**Files:**
- Modify: `web/apps/web/src/components/app/shell-nav.tsx:22-36` (the `place()` doc comment)

**Why:** `/{owner}/workspaces/{id}/snapshots` has three segments with `parts[1] === "workspaces"`, which is in `RESERVED`, so it falls through both three-segment special cases to `kind: "org"`. That is deliberate — the page renders its own back link and heading — but the doc comment explains the image and environment cases and is silent on this one, so the two `parts.length >= 3` branches read as a list that is missing an entry. **Do not add a `ws` branch.**

- [ ] **Step 1: Add the sentence**

Append to the `place()` doc comment, after the `environments` paragraph:

```
 *  `/{owner}/workspaces/{id}/…` deliberately does NOT get a branch of its own: a workspace's
 *  snapshots are a leaf, not a subject with tabs, and the page draws its own back link and
 *  heading (`workspaces/[id]/snapshots/page.tsx`). Three segments there stay `org` on purpose.
```

- [ ] **Step 2: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0 (a comment cannot break them; run them anyway before committing).

- [ ] **Step 3: Commit**

```bash
git add web/apps/web/src/components/app/shell-nav.tsx
git commit -m "Record why a workspace's snapshots page keeps the org chrome"
```

---

### Task 11: C1 + C2 + C3 — drop four dead fields from the api types

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` — `ApiWorkspace.live_state` (line 709), `ApiVolumeSummary.latest_ms` (line 941), `ApiCommitRecord.lineage` and `.region` (lines 966-975)
- Modify: `web/apps/web/src/lib/snapshot.test.ts:11-12`

**Why, per field:**
- `live_state: unknown` has no reader in `src`. The Rust field (`crates/workspaces/src/model.rs`) is hard-coded to `Value::Null` and its own doc says it is kept only because the web types name it — each side keeping it alive for the other. **The workspaces-api plan removes `Workspace.live_state` and the `api.rs` line that sets it; this task removes the web half, and the two must land together.** State that in the commit body.
- `lineage` is hard-coded `[]` by `snapshot_rows` and documented as wire-compat for older clients. The web is not an older client, so the *type* need not carry it.
- `region` is hard-coded `""` with no such excuse and no reader.
- `latest_ms` has no reader and one explicit warning against it (`environments/page.tsx:33`: *"`last_push_at`, never `latest_ms` — that one counts sync points, which are internal and never shown"*). Dropping it turns a convention into a compile error, which is the point.

Extra fields on the wire are ignored by these types (they are plain `type` aliases over parsed JSON, not validators), so removing a declaration removes nothing at runtime.

- [ ] **Step 1: Prove there are no readers**

```sh
cd web && grep -rn "live_state\|latest_ms\|\.lineage\|\.region" apps/web/src
```
Expected: `live_state` and `latest_ms` only in `lib/api.ts` and the `environments/page.tsx` comment; `lineage` only in `lib/api.ts` and `lib/snapshot.test.ts`; `.region` in `lib/api.ts` and in `env.region` on the environment header (that is `ApiEnvironment.region`, a DIFFERENT type — do not touch it). If any other reader shows up, stop and report it rather than deleting the field.

- [ ] **Step 2: Delete the four declarations**

In `lib/api.ts`: remove `live_state: unknown;` from `ApiWorkspace`; remove `latest_ms` and its doc comment from `ApiVolumeSummary`; remove `lineage: never[];` and `region: string;` and the `lineage` doc comment from `ApiCommitRecord`. Add to `ApiCommitRecord`'s existing header comment, beside its note about the undeclared `phase`:

```
 *  The wire also carries `lineage` (always `[]`) and `region` (always `""`), left undeclared for
 *  the same reason: nothing reads them, and a type that names a field invites one to.
```

Keep the warning comment at `environments/page.tsx:33-34` where the `last_push_at` substitution is made — it now explains a compile error instead of a convention.

- [ ] **Step 3: Update the wire-shape fixture**

`lib/snapshot.test.ts`'s `ROW` is deliberately the exact row `snapshot_rows` builds, so it keeps `lineage: []` and `region: ""` as *data* — but the type no longer requires them. Delete the `lineage: []` line (the review says it existed only to satisfy the type) and leave `region: ""` and `phase` where they are, as the comment above already frames them as fields the TS type does not declare. Extend that comment's last sentence to `…plus the `phase`, `region` and `lineage` the TS type does not declare.`

- [ ] **Step 4: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0. A tsc failure here means a reader Step 1 missed — fix the reader or restore the field, do not cast.

- [ ] **Step 5: Commit**

```bash
git add web/apps/web/src/lib/api.ts web/apps/web/src/lib/snapshot.test.ts
git commit -m "Drop four api fields nothing reads

live_state, latest_ms, lineage and region have no reader in the app.
live_state is hard-coded null on the server too and is kept there only
because these types named it; the workspaces-api change that removes
Workspace.live_state must land with this."
```

---

### Task 12: C4 — one `DeleteSnapshotDialog` for both kinds

**Files:**
- Modify: `web/apps/web/src/components/app/restore-dialog.tsx:86-133` (the workspace `DeleteSnapshotDialog` becomes the shared one)
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx:219-274` (delete the environment copy, import the shared one, update the call site around line 516)

**Why:** the two are the same trigger, the same `` aria-label={`Delete snapshot ${label}`} ``, the same `deleteVolumeCopy(1)` body, the same hidden field set and the same `label = message || id.slice(0,8)` rule, differing only in the action they bind and one trailing sentence. `archived-snapshots.tsx:25-26` already demonstrates the shape that collapses them. The three *restore* dialogs are NOT merged — they genuinely differ, and one component behind three flags reads worse than three honest ones.

- [ ] **Step 1: Generalise the dialog in `restore-dialog.tsx`**

Replace the existing `DeleteSnapshotDialog` with:

```tsx
/** Both kinds' action states are `{ ok?, error? }` plus fields this dialog never reads, so one
 *  shape drives both — the same idiom `archived-snapshots.tsx` uses. */
type DialogState = { ok?: true; error?: string } | null;
type DialogAction = (prev: DialogState, fd: FormData) => Promise<DialogState>;

/** Delete ONE snapshot. A snapshot is kept until it is explicitly deleted, and this is that
 *  delete: the bytes go with it, which is why the copy says so plainly rather than talking about
 *  a record. The live disk, if there still is one, is untouched.
 *
 *  One component for a workspace's and an environment's snapshots: same trigger, same label rule,
 *  same body, differing only in the action bound and one trailing sentence. */
export function DeleteSnapshotDialog({
  owner,
  id,
  snapshotId,
  label,
  action: act,
  note,
}: {
  owner: string;
  id: string;
  snapshotId: string;
  /** The message, or the short id when there is none — the dialog has to name ONE of several
   *  snapshots that may all be message-less. */
  label: string;
  action: DialogAction;
  /** One extra sentence after the standard body: what else this particular delete costs. */
  note?: React.ReactNode;
}) {
  const [state, action, pending] = useActionState<DialogState, FormData>(act, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" className="text-destructive" aria-label={`Delete snapshot ${label}`}>
          <Trash2 />
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Delete snapshot &ldquo;{label}&rdquo;?</DialogTitle>
            <DialogDescription>
              {deleteVolumeCopy(1)}
              {note}
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <input type="hidden" name="snapshotId" value={snapshotId} />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" variant="destructive" disabled={pending}>
              {pending && <Loader2 className="animate-spin" />}Delete
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
```

If `deleteWorkspaceSnapshot` is now unused in this file's imports, remove it from the import list — its caller passes it in.

- [ ] **Step 2: Update the workspace call site**

Find the existing `<DeleteSnapshotDialog …>` in the workspace snapshots page/component that renders it and add:

```tsx
  action={deleteWorkspaceSnapshot}
  note={<> The workspace itself is not affected.</>}
```

(keep the leading space inside the fragment — it separates the two sentences). Import `deleteWorkspaceSnapshot` at that call site if it is not already there. Run tsc to find the site if it is not obvious: removing the internal import makes it a compile error.

- [ ] **Step 3: Delete the environment copy and call the shared one**

In `env-snapshots.tsx`, delete the whole local `DeleteSnapshotDialog` (its doc comment included) and change the call site to:

```tsx
                  <DeleteSnapshotDialog
                    owner={owner}
                    id={id}
                    snapshotId={c.id}
                    // The id, not the word "snapshot": the dialog names ONE record among several
                    // that may all be message-less, and "Delete snapshot “snapshot”?" names none.
                    label={c.message || c.id.slice(0, 8)}
                    action={deleteEnvironmentSnapshot}
                    note={
                      isCurrent ? (
                        <>
                          {" "}
                          The environment itself is not affected. This is the snapshot the
                          environment currently sits on; its disk does not change, but the
                          snapshots will no longer show where it is.
                        </>
                      ) : (
                        <> The environment itself is not affected.</>
                      )
                    }
                  />
```

`isCurrent` is already computed one line above (`const isCurrent = c === current;`). Note the Task 5 wording ("the snapshots will no longer show where it is") — do not reintroduce "lineage". Add `import { DeleteSnapshotDialog } from "@/components/app/restore-dialog";` and drop the now-unused `Trash2`, `deleteVolumeCopy` and dialog-primitive imports **only if** nothing else in the file uses them (the file's own `RestoreDialog` still does — check with tsc/lint rather than by eye).

- [ ] **Step 4: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0. `lint` catches any import left unused.

- [ ] **Step 5: Check both dialogs by eye**

`bun run dev`: delete-snapshot on a workspace's snapshots page and on an environment's Snapshots tab — same trigger, same title, and the environment's `current` row carries the extra sentence.

- [ ] **Step 6: Commit**

```bash
git add web/apps/web/src/components/app/restore-dialog.tsx web/apps/web/src/components/app/env-snapshots.tsx
git commit -m "Collapse the two delete-snapshot dialogs into one"
```

---

### Task 13: C5 — `Notices` moves out of one of its two consumers

**Files:**
- Modify: `web/apps/web/src/components/app/workspace-list.tsx:171-188` (remove `Notices`), `:369` (import it)
- Modify: `web/apps/web/src/components/app/wsenv-state-badge.tsx` (add `Notices`)
- Modify: `web/apps/web/src/components/app/environment-list.tsx:11` (import from the new home)

**Why:** a shared presentational component lives inside one of its two consumers. Its logic already lives correctly in `lib/ws-status.ts` (`noticesFor`); only the eight-line renderer is misplaced. `wsenv-state-badge.tsx` already holds exactly this "one look, both kinds" role and its doc comment says so.

- [ ] **Step 1: Move the component**

Cut `Notices` and its doc comment from `workspace-list.tsx` and paste it at the bottom of `wsenv-state-badge.tsx`, unchanged except for the last sentence of its comment, which becomes: `Lives here rather than in either list, beside the badge it sits under — one look, both kinds.` Add `import { noticesFor } from "@/lib/ws-status";` to `wsenv-state-badge.tsx`.

`wsenv-state-badge.tsx` has no `"use client"` directive and `Notices` uses no hooks, so it stays a server component and both consumers (client components) may render it. Do not add `"use client"`.

- [ ] **Step 2: Point both consumers at the new home**

`workspace-list.tsx` and `environment-list.tsx` both import from `@/components/app/wsenv-state-badge` already — extend that existing import rather than adding a second line:

```tsx
import { Notices, WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
```

Remove `import { Notices } from "@/components/app/workspace-list";` from `environment-list.tsx` (line 11), and remove `noticesFor` from `workspace-list.tsx`'s imports if nothing else there uses it.

- [ ] **Step 3: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0; `lint` flags any stale import.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/components/app/wsenv-state-badge.tsx web/apps/web/src/components/app/workspace-list.tsx web/apps/web/src/components/app/environment-list.tsx
git commit -m "Move Notices next to the badge both lists share"
```

---

### Task 14: C7 — say why the settings page counts history rows

**Files:**
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/settings/page.tsx:21`

**Why:** the settings page passes `snapshots={page.history.length}` while the list page reads the api's `v.snapshots` — two sources for one number, and the copy is a promise (`deleteVolumeCopy(n)` says "Deletes N snapshots. This cannot be undone."). They agree today because both count non-transient snapshots. Not worth a refactor; worth not making the next person re-derive it from `snapshot_rows`.

- [ ] **Step 1: Add the comment**

```tsx
      {/* The history this page already holds, not the api's `snapshots` count the list page reads:
          the two are the same number — `snapshot_rows` returns exactly the pushes `snapshots`
          counts, transients excluded — and one round trip is already spent on the history. */}
      snapshots={page.history.length}
```

- [ ] **Step 2: Run the gates**

```sh
cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: all exit 0.

- [ ] **Step 3: Commit**

```bash
git add "web/apps/web/src/app/(shell)/[owner]/(org)/environments/[id]/settings/page.tsx"
git commit -m "Record why the settings page counts history rows"
```

---

## Final verification

- [ ] **All three gates green from a clean tree**

```sh
cd web && bun install && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test
```
Expected: lint "1 successful, 0 warnings"; tsc silent; `bun test` 0 fail, with 26 test files (25 − `env-page.test.ts` + `env-current.test.ts` + `packages-field.test.ts`) and 116 pass (105 − 1 + 8 + 3, adjusted for the assertions actually written).

- [ ] **Constraint sweep**

```sh
cd web && grep -rn "rounded-" apps/web/src | grep -v "rounded-full"     # expect no new hits
grep -rn "text-red-\|text-green-\|text-blue-\|bg-red-" apps/web/src     # expect none
grep -rn "lineage" apps/web/src --include="*.tsx"                       # expect comments only
git -C .. log --oneline -14 | cat                                       # expect 14 subjects, no attribution trailers
git -C .. log -14 --format=%B | grep -i "co-authored\|claude\|generated with"   # expect no output
```

- [ ] **Spec coverage check** — I1 Task 1, I2 Task 2, I3 Task 3, I4 Task 4, M1 Task 5, M2 Task 6, M3 Task 7, M4 Task 8, M5 Task 9, M6 Task 10, C1/C2/C3 Task 11, C4 Task 12, C5 Task 13, C6 Task 3, C7 Task 14, `live_state` Task 11, `react-markdown` audit decided above with no task. Nothing from "Good, leave alone" was touched: `AutoRefresh`, the push-pending state machine, `useDialogUntilSuccess`, `safeSegment`, `safeNext`, `lib/api-token.ts` and the three restore dialogs are all unchanged.
