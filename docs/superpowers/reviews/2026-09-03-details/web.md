# Web app code review — `web/apps/web`

Scope: every file under `web/apps/web/src` (app/, components/, lib/) plus the 25 test files, read
against `CLAUDE.md`'s "Web app" section, `web/apps/web/AGENTS.md`, and the Rust sources the types
mirror (`crates/workspaces/src/{api,model,crd,upstream}.rs`). Commit `4c7e94c9`.

Checks run once, all green:

```
bun run lint                                    → 1 successful, 0 warnings
bunx tsc --noEmit -p apps/web/tsconfig.json     → clean
bun test                                        → 105 pass / 0 fail, 25 files
```

Counts: **Critical 0 · Important 4 · Minor 6 · Cleanup 7**.

The security surface is the strongest part of this codebase and produced no findings worth
raising — details in "Good, leave alone" at the bottom, because the *absence* of a finding there
is itself the review's main result.

---

## Critical

None. No injection, no token leak, no unescaped API text, no open redirect, no missing
`server-only`, no destructive action reachable without the session cookie.

---

## Important

### I1. `volumeSnapshots` turns every failure into "No snapshots left to restore from"

`app/(shell)/[owner]/(org)/volume-actions.ts:16-19`

```ts
const r = await api.volumeHistory(token, name);
return r.ok ? r.value : [];
```

and the same at line 14: `if (typeof token !== "string") return [];`

This is precisely the failure `lib/require-api.ts:7-13` documents as having already happened once
in this product:

> `unauthorized` means the api rejected our token — the session outlived it. That is emphatically
> NOT "you have no keys" or "no such repo" … It has happened: an expired token made every list
> empty and every repo look deleted, with nothing logged, because a failed call was quietly turned
> into `[]`.

The consumer is `components/app/archived-snapshots.tsx:88-92`, which renders the empty array as:

```tsx
<p role="alert" …>No snapshots left to restore from.</p>
```

So an expired session, or a 5 s timeout on the api tier, tells someone standing in front of a
deleted workspace that the last copy of their data is gone. The volume listing that produced the
row said `snapshots: N > 0` moments earlier (`lib/archived.ts:31` filters on `v.snapshots > 0`),
so the page is contradicting itself.

**Cost if left:** the highest in this review. It is a false claim of data loss on the one screen
where data loss is the user's actual fear, and it is silent — nothing is logged, and the dialog
looks like it worked.

**Fix:** return the discriminated result instead of collapsing it.

```ts
export async function volumeSnapshots(name: string):
  Promise<{ ok: true; rows: ApiCommitRecord[] } | { ok: false; error: string }> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, error: token.error };
  const r = await api.volumeHistory(token, name);
  return r.ok ? { ok: true, rows: r.value } : { ok: false, error: r.message || "Could not read the snapshots." };
}
```

Then in `archived-snapshots.tsx` keep three states, not two: loading, failed (`role="alert"`,
destructive, "Could not read the snapshots — try again"), and genuinely empty. The Delete-volume
button in that same row must stay usable in the failed state; it is the one action whose meaning
does not depend on the list.

### I2. The environment header and the Snapshots tab disagree about which snapshot is current

`app/(shell)/[owner]/(org)/environments/[id]/layout.tsx:45`

```ts
const at = env ? (history.find((c) => c.id === env.restored_to) ?? history[0] ?? null) : null;
```

with the comment on line 76: *"Same rule the tab uses: `restored_to` when set, else the newest
record."*

It is not the same rule. `components/app/env-snapshots.tsx:315-322` computes `current` as: the
newest record that was pushed **after** `restore_requested_at` **and descends from** `restored_to`,
falling back to `restored_to` itself:

```ts
: (history.find((h) => snapshotTime(h) > since && descends(h, restored.id)) ?? restored);
```

The two answers differ in exactly the case the careful version exists for: an in-place restore
followed by a push. The header then says "at «before restore to X»" while the tab one click away
badges a newer record `current`. It also differs on the foreign-restore case — the tab renders
`foreignCurrent` honestly (`env-snapshots.tsx:326`, "no longer in this lineage"), while the header
silently falls through to `history[0]` and asserts the environment is on a record it is not.

**Cost if left:** the header is the more-read surface, and it is the one that is wrong. A wrong
"you are here" on a restore/rollback screen is how someone restores the wrong point.

**Fix:** lift the `current` computation out of `env-snapshots.tsx` into `lib/env-page.ts` (or a
sibling `lib/env-current.ts` — pure, testable, the same shape `ws-status.ts` and `snapshot-state.ts`
already take) and have both the layout and the tab call it. This is the one place in the app where
the same non-trivial decision is made twice with two different algorithms; the fix removes the
second one rather than syncing it. It should carry a test for: never-restored, restored-then-pushed,
restored-with-sibling-branch, and foreign `restored_to`.

### I3. `provenanceOf` reads a wire shape `/history` no longer sends

`lib/env-page.ts:5-13, 53, 58`

```ts
export type Provenance = { kind?: string; name?: string; services?: ApiService[] };
…
const newest = provenanceOf(history[0]?.state);
…
name: env?.name ?? volume?.display_name ?? newest.name ?? id,
```

The doc comment points at `crates/workspaces/src/upstream.rs::Provenance`, which does carry
`name`. But `/v1/volumes/{name}/history` does not serialize `Provenance` any more — `snapshot_rows`
(`crates/workspaces/src/api.rs:2421-2443`) emits `spec.state`, which is `crd::SnapshotState`
(`crd.rs:318-335`, `#[serde(tag = "kind", rename_all = "camelCase")]`): `{kind, image, packages,
resources, quotaGb, attachedEnvironment}` or `{kind, services, quotaGb}`. There is no `name` field
in either variant, ever.

So `newest.name` is permanently `undefined`, and the third fallback in the `name` chain is dead.
It half-works only by coincidence: `kind` and `services` happen to exist on the environment
variant, which is why nothing has ever looked broken. The web's own `ApiCommitRecord.state` is
correctly typed as `SnapshotState` (`lib/api.ts:966`), so the file contradicts its own module's
types — `provenanceOf` takes `unknown` and casts, which is what let the drift through the compiler.

`lib/env-page.test.ts:8` then asserts `provenanceOf({ name: "web", services: [] })` round-trips —
a test that pins a shape the server cannot produce.

**Cost if left:** low today, misleading forever. The archived-environment page's name fallback is
one `display_name` outage away from showing a raw volume id while a correct-looking line of code
sits there claiming to prevent it.

**Fix:** delete `Provenance`, `provenanceOf`, the `newest` line and the test. `name` becomes
`env?.name ?? volume?.display_name ?? id`. If a provenance name is genuinely wanted for an archived
row, the fix belongs on the server: add it to `snapshot_rows`. Don't re-cast `unknown` here.

### I4. Two sequential API round trips on both list pages

`app/(shell)/[owner]/(org)/workspaces/page.tsx:16` then `:27`, and
`app/(shell)/[owner]/(org)/environments/page.tsx:17` then `:29`

```ts
const list = await listWorkspaces(token, …);   // await
…
const volumes = await listVolumes(token, "workspace", …);  // then await
```

`listVolumes` does not depend on `list` — both are keyed only off `token` and `owner`. Each call
carries `TIMEOUT_MS = 5_000` (`lib/api.ts:20`), so the worst case for these two pages is a 10 s
render, and the typical case is two serial RTTs to the api tier where one would do.

The org dashboard already gets this right — `(org)/page.tsx:157` does
`await Promise.all([repos, events, workspaces, environments])` — and so does `loadEnvPage`
(`lib/env-page.ts:44`). These two pages are the outliers, and they are the two most-visited pages
of the workspaces product.

**Cost if left:** doubles the p50 of the two busiest pages, and doubles the tail. Also multiplied
by `AutoRefresh`'s 10 s poll on the same pages (and 2 s while any row is `creating`), so it is
sustained load, not a one-off.

**Fix:** one line each.

```ts
const [list, volumes] = await Promise.all([
  listWorkspaces(token, scope),
  listVolumes(token, "workspace", scope),
]);
```

The error handling below is unchanged — `list` still throws/redirects/404s, `volumes` still
degrades to `[]`.

Note the *good* half of this design deserves protecting: `volume-actions.ts` deliberately does
**not** fetch a history per archived row (`volume-actions.ts:8-12`), so there is no N+1 here. The
`listVolumes` + `volumeHistory`-per-row waterfall the brief asked about does not exist.

---

## Minor

### M1. "lineage" is outside the product vocabulary, in user-facing copy

`components/app/env-snapshots.tsx:434, 450, 255`

```
"No snapshots yet — take one to start the lineage"
"… is no longer in this lineage — changes since are not snapshotted"
"… but the lineage will no longer show where it is."
```

The stated vocabulary is workspace / environment, push, snapshot, sync point (never shown),
restore, clone, delete. "Lineage" is a fourth word for the snapshot chain, and it appears only
here — the rest of the app says "snapshots". `lib/archived.ts:10` even states the rule
("There is no commit and no pin"), and `lib/api.ts:971` keeps a wire field called `lineage` that is
always `[]`, so the word already means something else internally.

**Cost:** a word the user has to learn that buys nothing.

**Fix:** "take one to start" / "is no longer among this environment's snapshots" / "but the
snapshots will no longer show where it is". Comments may keep the word; the screen should not.
`docs`-side: worth a grep gate if one exists.

### M2. `ToggleForm` offers "Stop" for a workspace that is `creating`, `error` or `deleted`

`components/app/workspace-list.tsx:346` — `running={w.state !== "stopped"}`

Every non-`stopped` state gets the Stop button, including `creating` (nothing to stop yet),
`error` and `deleted`. The environment side gets this right:
`components/app/env-actions.tsx:206` — `running={state === "running"}` — a positive test.

**Cost:** a button that errors instead of a button that isn't there; the codebase's own stated
principle (`workspace-list.tsx:359`, *"A button that only ever errors is worse than no button"*).

**Fix:** `running={w.state === "ready"}`, matching the environment's positive test. Consider
disabling the toggle entirely for `creating`/`deleted`.

### M3. Restore of a workspace whose snapshot froze an empty package list silently keeps it empty — correctly, but by accident

`app/(shell)/[owner]/(org)/workspaces/actions.ts:71-73` with
`components/app/restore-dialog.tsx:63` / `archived-snapshots.tsx:117`

```ts
const packages = formData.has("packages")
  ? String(formData.get("packages")).split(",").map(p => p.trim()).filter(Boolean)
  : undefined;
```

When the frozen state has `packages: []`, the input renders with `defaultValue=""`, the field is
present, and the action sends `packages: []`. `api.restoreWorkspace` (`lib/api.ts:876`) spreads
`...(extra?.packages ? { packages: extra.packages } : {})` — and `[]` is truthy, so an explicit
empty list goes on the wire. That happens to be right (the snapshot had none), but the comment on
line 872 says the opposite intent: *"an omitted one must stay off the wire entirely"*. The
`has()`-vs-truthiness split means one of the two guards is doing nothing.

**Cost:** none today; a trap for the next edit. If someone "fixes" the `[]`-is-truthy line to
`extra?.packages?.length`, restoring a package-less snapshot silently starts inheriting the
snapshot's own list instead of the empty one the user saw and accepted.

**Fix:** make the intent explicit in one place — `...(extra?.packages !== undefined ? { packages: extra.packages } : {})`
— and drop the redundant truthiness read. One test in `lib/` covering "empty field sends `[]`, absent
field sends nothing" would pin it.

### M4. `restore-keep-message` is a hardcoded DOM id inside a per-row dialog

`components/app/env-snapshots.tsx:174, 178`

`RestoreDialog` is rendered once per history row (line 519), each with the same
`id="restore-keep-message"` and matching `htmlFor`. It happens to be safe because Radix unmounts
closed `DialogContent`, so only one is ever in the DOM — but that is a property of the dialog
library, not of this code, and `forceMount` or a future portal change breaks it into duplicate ids
(an a11y violation: the label then points at whichever comes first).

The same file already does this correctly elsewhere: `archived-snapshots.tsx:82` uses
`id={`snap-${row.id}`}`.

**Fix:** `const uid = useId()` and interpolate, or `id={`restore-keep-${snapshot.id}`}`.

### M5. `stale` push warning is announced by a `role="alert"` that also carries the retry advice, but the "uploading…" state has no live region

`components/app/env-snapshots.tsx:482-486` (the pending node) and `:534-538` (the stale alert)

The pending row renders `uploading…` as plain text inside `<span>`. A screen-reader user who
submitted the push hears nothing when the row appears, nothing when it lands, and then — five
minutes later — a `role="alert"` interrupting them. The transition that matters (landed) is silent;
the transition that rarely happens (timed out) is the loud one.

**Cost:** the push flow is unusable non-visually. Low frequency, real.

**Fix:** wrap the pending node's status span in `role="status"` (polite), which announces both its
appearance and — because the node unmounts on landing — pair it with a `role="status"` on the
`current` badge, or add a visually-hidden "Snapshot taken" line keyed on `asked` going null. The
`role="alert"` on the stale message is correct as is.

### M6. `place()` gives a workspace's snapshots page the org tab row, where an environment's gets its own

`components/app/shell-nav.tsx:37-54`

`/{owner}/workspaces/{id}/snapshots` has three segments with `parts[1] === "workspaces"`, which is
in `RESERVED` (`lib/reserved.ts`), so it falls through both the `registries` and `environments`
special cases and lands on `kind: "org"`. The page compensates by rendering its own back link and
heading (`workspaces/[id]/snapshots/page.tsx:34-46`).

That is defensible — a workspace's snapshots are a leaf, not a subject with tabs — but it means the
shell has one deliberate three-segment shape it does not know about, and the two `parts.length >= 3`
branches read as a list that should have three entries and has two. The `place()` doc comment
(lines 21-35) explains the image and environment cases and is silent on this one.

**Fix:** cheapest is a sentence in the `place()` comment saying workspaces deliberately stay `org`
and why. Do **not** add an `ws` branch — the page owns its chrome and that is fine.

---

## Cleanup

### C1. `ApiWorkspace.live_state` is dead on both sides

`lib/api.ts:709` — `live_state: unknown;`

No reader anywhere in `src` (grepped). The Rust field
(`crates/workspaces/src/model.rs:78-84`) is hard-coded to `Value::Null` at `api.rs:409` and its own
doc says:

> This field stays `null` and is kept only because the web types still name it.

The two sides are each keeping it alive for the other. Drop it from `lib/api.ts`, then drop
`Workspace.live_state` and the `api.rs:409` line in the same PR.

### C2. `ApiCommitRecord.lineage` and `.region` are always empty

`lib/api.ts:966-975`. `snapshot_rows` (`api.rs:2430-2431`) hard-codes `"lineage": []` and
`"region": ""`. Neither has a reader in the app. `lineage` is already documented as wire-compat for
older clients — the web is not an older client, so the *type* need not carry it. `region` has no
such excuse.

Drop both from the type. `lib/snapshot.test.ts:11` sets `lineage: []` only to satisfy the type and
can lose the line.

### C3. `ApiVolumeSummary.latest_ms` has no reader and one explicit warning against it

`lib/api.ts:941`, and `environments/page.tsx:33` says *"`last_push_at`, never `latest_ms` — that
one counts sync points, which are internal and never shown."* The field exists solely to be warned
about. Sync points are never shown, so nothing in the web can ever legitimately read it.

Drop it from the type and keep the warning comment where the substitution is made. This turns a
convention into a compile error, which is the point.

### C4. Three restore dialogs and three delete-snapshot dialogs

| what | file:line |
|---|---|
| `RestoreDialog` (workspace, one snapshot, no picker) | `components/app/restore-dialog.tsx:26` |
| `RestoreDialog` (archived row, fetches + picks a snapshot) | `components/app/archived-snapshots.tsx:37` |
| `RestoreDialog` (environment, in-place *or* new) | `components/app/env-snapshots.tsx:113` |
| `DeleteSnapshotDialog` (workspace) | `components/app/restore-dialog.tsx:88` |
| `DeleteSnapshotDialog` (environment) | `components/app/env-snapshots.tsx:220` |
| `DeleteVolumeDialog` / `DeleteSnapshotsDialog` | `archived-snapshots.tsx:148` / `env-actions.tsx:75` |

The two `DeleteSnapshotDialog`s are the clearest duplication: same trigger, same
`aria-label={`Delete snapshot ${label}`}`, same `deleteVolumeCopy(1)` body, same hidden field set,
same `label = message || id.slice(0,8)` rule — differing only in which action they bind and one
trailing sentence. They are worth collapsing into one component taking `{ action, extra }`, in the
way `archived-snapshots.tsx:26-28` already demonstrates with its `DialogAction` type:

```ts
type DialogState = { ok?: true; error?: string } | null;
type DialogAction = (prev: DialogState, fd: FormData) => Promise<DialogState>;
```

The three restore dialogs are **not** worth merging. They genuinely differ — no picker / picker /
in-place-vs-new with a safety-snapshot sub-flow — and a single component behind three flags would
be harder to read than three honest ones. Leave them; take the delete pair.

### C5. `Notices` is exported from `workspace-list.tsx` and imported by `environment-list.tsx`

`workspace-list.tsx:174` → `environment-list.tsx:10`

A shared, pure-ish presentational component living in one of its two consumers. Its logic already
lives correctly in `lib/ws-status.ts` (`noticesFor`); only the eight-line renderer is misplaced.

Move it next to the badge it sits beside — `components/app/wsenv-state-badge.tsx` already holds
exactly this "one look, both kinds" role and its doc comment says so.

### C6. `env-page.test.ts` tests only the function I3 recommends deleting

`lib/env-page.test.ts` is four assertions on `provenanceOf`, three of which are "not an object →
`{}`". If I3 is taken the file goes with it. If I3 is deferred, the file is still testing the wrong
thing: the interesting behaviour of `lib/env-page.ts` is `loadEnvPage`'s archived fallback chain,
which has no test at all.

### C7. `EnvSettings` receives `snapshots={page.history.length}` where the list pages use `v.snapshots`

`environments/[id]/settings/page.tsx:20` vs `environments/page.tsx:35`

Two different counts of the same thing: the settings page counts history rows it already has, the
list page reads the api's `snapshots` field. They agree today (both count non-transient snapshots).
They are still two sources for one number, and the copy is a promise — `deleteVolumeCopy(n)` says
"Deletes N snapshots. This cannot be undone."

Not worth a refactor; worth a one-line comment at the settings page saying the two are equivalent
and why, so the next person does not have to re-derive it from `snapshot_rows`.

---

## Good, leave alone

The security review produced no findings, and that is the result, not an omission. Specifically:

- **Token handling is exemplary.** `lib/api-token.ts:1` is `server-only` and reads the api bearer
  from the *encrypted* JWT rather than from `auth()`, with the reason written down: the session
  object is what `/api/auth/session` hands the browser, so a credential on it would be readable by
  any client script. The token appears in no URL, no log line, no error message, and no client
  component prop. `lib/api.ts` never logs.
- **`safeSegment` is applied at the right boundary, for the right reason.** Every server action
  that builds a `revalidatePath` pattern from FormData validates first
  (`workspaces/actions.ts:26`, `environments/actions.ts:22`, and every sibling), and
  `lib/slug.ts:8` matches the server's own `valid_segment`. The one field that legitimately
  contains `/` — a branch name — gets its own rule with the reasoning inline
  (`workspaces/actions.ts:174-176`), rather than being forced through a check that would reject
  valid input.
- **Open redirects are closed and tested.** `safeNext` (`login/destination.ts:7`) rejects `//`,
  `/\`, and absolute URLs, and `destination.test.ts:43` exercises all of them — including the
  emailed-`next` path, whose test comment correctly identifies it as attacker-shaped input.
- **The magic-link redemption is a POST-only Server Action** (`verify/[token]/actions.ts:9-14`)
  with the reasoning spelled out: a GET link that signs a browser in is a login-CSRF. Server
  Actions give CSRF protection by origin check, and every destructive path in the app is one.
- **Exactly one `dangerouslySetInnerHTML`** (`components/repo/code-block.tsx:16`), fed by Shiki's
  `codeToHtml`, which escapes its input — and the file says so. Every API-supplied string,
  including the `role="alert"` error messages, goes through JSX children and is escaped.
- **`listOrSignIn` (`lib/require-api.ts`) is the right abstraction with the right scar tissue.**
  It distinguishes `unauthorized` from empty, and the comment records the incident that made it
  necessary. Finding I1 is the one place that lesson was not applied — everywhere else it is.
- **404-as-403 is handled deliberately.** `lib/api.ts:101-102` documents that the api answers 404
  for a namespace the caller may not act in, and the pages render it as one. `lib/session.ts:20-27`
  is emphatic that identity is never permission.

Beyond security:

- **`lib/` is where the decisions live, and they are tested.** `ws-status.ts`, `snapshot-state.ts`,
  `archived.ts`, `pending-push.ts`, `snapshot.ts` are all pure, all single-purpose, all with a
  comment naming the bug that produced them (`snapshot.ts` on `createdAt` vs `created_at` and its
  eight silent `Invalid Date`s is the best of these). 105 tests, and they assert behaviour rather
  than shape.
- **`AutoRefresh`** (`components/app/auto-refresh.tsx`) — visibility-gated, mounted only by pages
  whose state changes without the user, with a fast 2 s timer that exists exactly as long as a
  `creating` row does. The comment explains what the shell-wide version cost. Do not touch this.
- **The push-pending state machine** in `env-snapshots.tsx:283-310`. Derived during render rather
  than in an effect, `had` captured at submit rather than at result, cleared once and never
  re-derived so a later deletion cannot resurrect "uploading…". Three separate real bugs are
  designed out, each named in a comment.
- **`useDialogUntilSuccess`**, and `CloneDialog`'s deliberate opt-out of it
  (`workspace-list.tsx:79-81`) so the one success that must be *read* does not close the dialog.
- **Design tokens are honoured throughout.** Zero raw Tailwind palette colours
  (`text-red-500` and friends) and zero `rounded-*` classes anywhere in `src` — `--radius: 0` holds.
  `lib/utils.ts`'s `extendTailwindMerge` for the custom type scale, with the "font is too big"
  incident recorded, is exactly the kind of thing that otherwise gets re-broken annually.
- **`place()`'s reserved-name coupling** is sound and documented at both ends
  (`lib/reserved.ts`, `shell-nav.tsx:21-35`), including the one entry — `ci` — kept only to stay in
  step with the server.
