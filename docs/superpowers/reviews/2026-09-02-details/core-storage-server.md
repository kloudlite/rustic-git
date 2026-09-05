# Review: crates/{core,storage,gitbase,git,app} + bins/server

Read-only. Scope: routing/ownership/auth/pool/store/index/cache/git-protocol + the server binary.
Every finding below was verified against the code at the cited line; guesses were dropped.

Overall: this is unusually careful code. The routing-before-auth contract, the lease/fence
handling, the index marker fail-closed rules and the pool's single-flight are all correct as
documented, and the tests cover the failure modes the comments name. The findings are a short list.

**Counts — Critical 0, High 1, Medium 3, Low 6.**

---

## High

### H1. upload-pack negotiation body is capped at `max_body` (2 GiB), not at negotiation size
- Category: correctness / DoS
- `bins/server/src/router/git.rs:126-130` (`read_body`), used at `git.rs:415`
- The doc comment on `read_body` says "Upload-pack only: its request is the negotiation,
  kilobytes" — but the cap it passes is `max_body()`, which defaults to 2 GiB
  (`crates/core/src/httpx.rs:105-110`). `axum::body::to_bytes` **buffers** that in memory, and the
  handler is reachable **anonymously** on any public repo (`git.rs:411` → `open(..., read_only=true)`
  → `authorize(None, owner, public && read_only)`). A handful of concurrent 2 GiB
  `git-upload-pack` POSTs OOMs the pod — and the same comment block correctly identifies why that
  is worse here than elsewhere: an OOM "moves repo ownership … on an attacker's schedule".
  Nothing else bounds it: `router()` (`router/mod.rs:15-24`) adds no `DefaultBodyLimit`, and
  `Body` is extracted raw, so axum's extractor default never applies. Unbounded `wants`/`haves`
  vectors (`protocol/upload/mod.rs:102-159`) are a second-order amplifier of the same body.
- Fix: give the negotiation its own constant (a few MiB is generous — git's largest real
  negotiation is thousands of pkt-lines) and pass it to `to_bytes` on the upload-pack path;
  keep `max_body` for `receive_pack`'s streamed body, which is already correct
  (`live_body`, `git.rs:137-150`).
- Test gap: `tests/git_http_limits.rs:81` covers the push cap; nothing covers this one.

## Medium

### M1. Newline injection into listing-marker bodies via a repo description
- Category: correctness / data integrity (spoofing)
- `crates/storage/src/index.rs:60-90` (`body`/`decode`); reachable from
  `bins/server/src/browse_api/admin.rs:172-177` and `:192-203`, whose input is validated only for
  length at `crates/api/src/repos.rs:60-67`.
- The marker body is `k=v` lines and `decode` parses **per line**; `description` is placed last so
  it may contain `=`, but nothing stops it containing `\n`. A description of
  `"hi\ncreated_by=someone.else\ncreated_ms=0"` writes those as real fields, and every listing then
  renders the forged creator. Visibility is *not* affected (`public` comes from the path, per the
  module doc), so this is integrity, not authorization.
- Fix: reject control characters (at minimum `\n`, `\r`) in `check_description` — the root-cause
  spot, since both `create` and `description` route through it — and, belt-and-braces, escape or
  refuse newlines in `index::body` so no future writer can reintroduce it.

### M2. Unauthenticated claim amplification against the single elected writer
- Category: security / availability
- `crates/app/src/lib.rs:411-422` (the `ponytail:` note names the ceiling honestly)
- `route()` runs before authentication (by design — `router/route.rs:280-284`) and CLAIMS any repo
  key the map does not name, whether or not the name exists. A spray of distinct invented
  `/{owner}/{name}/info/refs` paths therefore turns into one leader map write per name per
  `LEASE_TTL`, from an anonymous client, against the one node that must stay responsive for the
  whole fleet's routing. `prune_once` (`lib.rs:599-611`) then scans and deletes them all.
- The existing comment proposes the right fix; it is worth doing rather than deferring: a per-node
  token bucket on claims for names whose prefix is empty (`pool.exists` is already the check
  `force_claim` uses at `lib.rs:519-523`), or claim-only-if-exists for the non-forced path with the
  create routes exempted.

### M3. `/metrics` is served on the peer listener without the peer secret
- Category: security (information disclosure)
- `bins/server/src/router/route.rs:557-562`
- `trust_peer` returns early for `/metrics` so Prometheus can scrape it. The peer port's own doc
  (`crates/core/src/peer.rs:10-13`) states the cluster runs with `networkPolicy: none` and "any pod
  can reach them" — so any pod can read repo names, ownership state, fence counts and pack byte
  totals. Not a data-plane compromise, but it is the one hole in a listener whose whole premise is
  the shared secret.
- Fix: bind metrics to its own listener/port, or accept the secret on `/metrics` too and configure
  Prometheus with it. If neither is wanted, say so in the comment — the current comment argues only
  that the *routing* invariant is not in play, which is a different question from disclosure.

## Low

### L1. `mem://` is accepted as a fleet object store
- `crates/storage/src/config.rs:119-129`, called from `bins/server/src/main.rs:58`
- `fleet_store_ok` refuses `file://` (no conditional `Update`) but allows `mem://`. In a real
  multi-pod deployment `InMemory` is per-process: every pod takes epoch 1 of its own lease, every
  pod is leader, and every pod opens every database — precisely the two-writer bug the guard
  exists to prevent. It is fine for the in-process test fleet, which is what the message means.
- Fix: gate `mem://` on `#[cfg(test)]`/an explicit `KLOUDLITE_ALLOW_MEM_FLEET`, or refuse it here
  and let the test fleet call the inner check directly.

### L2. `forget_pack_public` is a one-caller `pub` alias for a private method
- `crates/storage/src/store.rs:620-622`; sole caller `crates/git/src/gc.rs:135`
- Fix: make `forget_pack` `pub` and delete the wrapper.

### L3. `ssh_fingerprint` is duplicated body-identically across crates
- `bins/server/src/boot.rs:38-42` and `crates/api/src/credentials.rs` (the comment says so and asks
  for manual mirroring). Two copies of a security-relevant parse is the kind of thing that drifts.
- Fix: a tiny `kloudlite-ssh-keys` helper, or move it into `core` behind a feature so `storage`
  keeps its no-ssh-dependency property.

### L4. `revoke_tokens_for` is a full LIST plus one GET per token
- `crates/storage/src/auth.rs:117-131`. Documented as admin-scale and it is, but it grows with the
  fleet's whole token set. Only worth an index if `admin` ever runs it on a schedule.

### L5. Client-visible error text on the `Other` io kind
- `bins/server/src/router/git.rs:498-514` + `limits.rs:32-34`: `is_client_fault` treats
  `io::ErrorKind::Other` as the client's fault and echoes `e.to_string()` in a 400. The OS-error
  cases are correctly excluded and tested (`git.rs:540-552`), but `Other` is also produced by
  `Streamed`/`Tee`/`live_body`, so an internal message can reach the client. Low impact (the
  strings are literals today) — worth pinning with a test if the set of `Other` producers grows.

### L6. Test gaps on load-bearing paths
- No test that the upload-pack negotiation body is bounded (see H1).
- No test that a description containing a newline cannot forge marker fields (see M1).
- No test for the credential-half-mismatch refusal in `open()` (`git.rs:44-46`,
  `httpx::user_names`): a valid token presented under a wrong username must 401, and only the pure
  `user_names` half is exercised.

---

## Architecture notes

- Ownership/lease/fence design is sound and does not need restructuring. The one structural gap is
  that **everything expensive happens before authentication** (claim in `App::route`, body buffering
  in `read_body`). Moving the claim behind an existence check, and the body cap down to what the
  route actually needs, closes H1 and M2 with no new machinery.
- `bins/server/src/browse_api/*` authorizes on the peer secret alone for the write routes
  (`admin.rs:23-27` argues this well) while the read routes go through `open_ro`. That split is
  correct but invisible from the router: a new write route that forgets it is indistinguishable.
  A `peer_write()` marker fn (even one that only documents and asserts `Trusted` is present) would
  make the intent checkable the way `BROWSE_TAILS` already makes routing checkable.
- The three near-duplicate credential decoders were already consolidated into `core::httpx`; the
  remaining duplication is `ssh_fingerprint` (L3). Finishing that consolidation removes the last
  hand-mirrored security-relevant function in this area.
