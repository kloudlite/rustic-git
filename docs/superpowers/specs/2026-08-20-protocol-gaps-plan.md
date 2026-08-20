# Closing the git protocol gaps

**Goal:** make this server behave like a git server clients already know how to
talk to, so nothing has to be worked around on the client side.

Ordered by *who is blocked today*, not by how interesting the work is.

---

## Phase 1 — Shallow and partial clone

**Why first:** `actions/checkout` defaults to `fetch-depth: 1`. Any normal CI
hits `fatal: Server does not support shallow requests` and stops. CI Triggers are
a first-class product concept, so this is self-inflicted.

Measured today:

| Client asks | Server answers |
|---|---|
| `--depth 1` | `fatal: Server does not support shallow requests` |
| `--filter=blob:none` | `warning: filtering not recognized by server, ignoring` → full transfer |

### 1a. Shallow fetch

The good news: **the server stays stateless.** Git's shallow boundary lives in
the client's `.git/shallow`, and the client re-sends it as `shallow <oid>` lines
on every request. Nothing new to store.

1. Advertise it: `fetch=shallow wait-for-done` (`upload.rs:14`).
2. Parse, instead of rejecting (`upload.rs:139`):
   - `deepen <n>` — depth from each want
   - `deepen-relative` — depth from the client's existing boundary
   - `deepen-since <unix>` — cut by date
   - `deepen-not <ref>` — cut at a ref
   - `shallow <oid>` — what the client already treats as a boundary
3. Compute the boundary: walk `n` commits from each want; a commit whose parents
   are cut off is a boundary commit.
4. Emit a `shallow-info` section *before* `acknowledgments`:
   - `shallow <oid>` for new boundary commits
   - `unshallow <oid>` for commits the client had as a boundary that are now
     complete (this is what `--unshallow` is)
5. Bound the pack walk at the boundary. `write_pack` already takes a hide-set and
   `reachable_set_hiding` already exists — hide the boundary commits' parents.

**Risk:** getting the boundary wrong produces a repo that looks fine until
someone runs `git log` and hits a missing object. Mitigate with tests that clone
at several depths and assert both the commit count *and* that `git fsck` and
`git log` complete.

**Also:** pushing *from* a shallow clone is a separate feature (the client sends
its own `shallow` lines to receive-pack). Out of scope here — CI checks out, runs,
and rarely pushes. Worth stating so nobody assumes it works.

### 1b. Partial clone

Bigger than shallow, because it changes an invariant: the client is allowed to be
*missing* objects and will come back for them later.

1. Advertise `filter` in the fetch capabilities.
2. Support `filter blob:none` and `filter blob:limit=<n>` — omit blobs when
   building the pack.
3. Support the follow-up fetch, where the client asks for specific blobs by oid.
   This needs the `want` rule at `upload.rs:186` relaxed: today only ref tips may
   be wanted, deliberately, because objects are shared across a fork network.
   A promisor fetch has to allow blobs reachable from this repo's refs —
   *reachable from*, not merely *existing in the pool*, or it becomes an existence
   oracle for a sibling repo.

**Do 1a first and ship it.** It unblocks CI on its own, and 1b's security-sensitive
part deserves its own review rather than riding along.

---

## Phase 2 — Cheap compatibility wins

Small, independent, low risk. Roughly a day together.

### `atomic` on push (`receive.rs:10`)

`update_refs` is **already** all-or-nothing. We simply never say so, and a client
that needs the guarantee has no way to ask. Add `atomic` to `CAPS`.

Note the honest wrinkle: we behave atomically *whether or not* the client asks,
which is stricter than git. Advertising it makes the behaviour visible instead of
surprising.

### `peel` in ls-refs (`upload.rs:114`)

For an annotated tag, also emit the commit it points at:
`<tag-oid> refs/tags/v1 peeled:<commit-oid>`. Without it `git ls-remote` can't
show `^{}` entries and clients peel with an extra round trip.

### `include-tag` (`upload.rs:162`)

After the pack contents are decided, add any tag whose target is in the pack.
This is why `git clone` normally brings tags along — today it doesn't.

### `ref-in-want` (`upload.rs:162`)

Accept `want-ref <refname>`, resolve it server-side, return a `wanted-refs`
section. Removes the ls-refs round trip and closes the race where a ref moves
between discovery and fetch.

---

## Phase 3 — Push extras

### `push-options`

Accept `git push -o key=value` and carry the options through. They're inert until
something consumes them — but CI Triggers are exactly the consumer, so this is
worth having in place before that lands rather than retrofitting.

### `report-status-v2`

Richer per-ref push results. Mechanical; do it when something needs to report
more than "ok" or one line of error.

### `push-cert`

Signed pushes. Needs GPG verification and a policy for which keys are trusted.
**Not planned** — no demand, and a signature we verify carelessly is worse than
no signature.

---

## Phase 4 — SHA-256 repositories

Not a capability flag; a different object format. It touches hashing, storage
layout, pack format and every oid parse in the tree, and a repo's format is fixed
at creation, so it also needs a per-repo setting and a creation-time choice.

**Not planned.** Revisit when a customer asks, and treat it as its own project
rather than a checkbox.

---

## Sequence

1. **Shallow** — unblocks CI. Ship alone.
2. **atomic + peel + include-tag + ref-in-want** — one small batch.
3. **Partial clone** — its own review, because of the want-rule change.
4. **push-options** — before CI Triggers needs it.
5. `report-status-v2` when something reports more; `push-cert` and SHA-256 on demand.

## How each phase is judged done

Not "the code compiles" — a real client does the real thing:

- `git clone --depth 1`, then `--depth 5`, then `git fetch --unshallow`, each
  followed by `git fsck` and a full `git log`.
- `git clone --filter=blob:none`, then open a file and confirm the lazy fetch.
- `git push --atomic` with one bad ref: nothing lands.
- `git ls-remote` shows `^{}` for an annotated tag.
- `git clone` brings tags with it.
- `git push -o ci.skip=true` is accepted.
