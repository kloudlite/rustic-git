# Review hardening — 18 September 2026

Baseline: `desktop-login` at `695f8484d1c60bef7470a0d7243910b7cb9c4777`. This tracks the 16 findings in the codebase review dated 18 September. Planning and independent review use Astra; implementation uses Luna. Application changes are not complete until the carrying commit passes its required checks. No deployment or live resource mutation is part of this work.

## Findings

| ID | Status | Change | Required evidence |
|---|---|---|---|
| R01 | In progress | Confine aggregate worktree reads; diff symlinks as link text; cover derived views. | External and parent-directory symlinks cannot disclose sentinel bytes; ordinary tracked/untracked diffs still work. |
| R02 | In progress | Hide agent trees from main-session sandbox commands. | Real bubblewrap read/write refusal; each agent retains its own intended tree. |
| R03 | In progress | Share bench parent-login and bench-liveness admission across routers. | Both routers refuse revoked/deleted/stopped credentials within the existing cache bound; normal authorization remains enforced. |
| R04 | In progress | Coordinate publication and GC with CAS state and immutable blob generations. | Forced publication/deletion races, stale deletion after reupload, CAS ABA, failure/retry, legacy compatibility. |
| R05 | In progress | Publish usage only after every expected child is measured successfully. | Mixed success/failure, directory and entry errors, overflow; previous complete value and timestamp survive. |
| R06 | In progress | Carry confined requested cwd into sandbox execution. | Nested `pwd` and relative writes with and without wrapping. |
| R07 | In progress | Prepare and own fresh checkout in staging before publishing; converge quota on retry. | Interrupted creation/chown retries; reject symlink/non-subvolume staging; uid 1000 can write; restored ownership preserved. |
| R08 | In progress | Query all active persisted exchanges for recovery/deadlines. | An open ask older than 500 completed rows resumes or expires after reopening the log. |
| R09 | In progress | Share exchange state types and terminal/active predicates. | Every state checked in queue, pending cards, footer, and history outcomes; shared runtime module included in the bench image. |
| R10 | In progress | Bound HTTP requests with total deadlines and single settlement. | No headers, stalled body, mid-response abort, success and keepalive reuse. |
| R11 | In progress | Persist watch events in an object-store outbox before advancing watcher state; drain with retries. | Failed phase/deletion batches retain deterministic IDs; restart, ambiguous acknowledgments, concurrent drains, and poison-record progress verified. |
| R12 | Planned | Gate bench publication on locked harness checks and exact source identity. | Deliberately failing test blocks mocked image publication; stale gate record cannot authorize bypass. |
| R13 | In progress | Build source and run isolated, observable renderer boot verification. | Unique profile/port; child failures fail required mode; populated mock app and transcript actually exercised. |
| R14 | Planned | Coordinate rollout and every probe entrypoint; wait for applied workloads. | Scheduled/manual/pending probes block rollout; unready admin blocks success; concurrent starts covered. |
| R15 | Planned | Replace retired bench-shell package mutation with supported coverage. | Authenticated workspace mutation and deliberate bench-shell refusal; explicit command result where required. |
| R16 | Planned | Restore only run-owned node mutations with conditional patches. | Operator-owned and concurrently changed values survive cleanup; run-owned originals restored. |

Review checkpoints: R03 implementation is committed as `694715b2`; its source wiring shares the workspace admission state and cache, but real callback/middleware and cache-expiry evidence is still required. IDE changes are committed as `8957ed1e`; review found that main-tree diff/numstat confinement must also reject resolved aliases into `.agents`, and the ignored bubblewrap test needs corrected write expectations plus a fixture outside the masked `/tmp`. These commits are not closure evidence.

Follow-up source review: `2b712a61` uses descriptor-relative reads with no-follow parent components and rejects static checkout/staging symlinks. Runtime sandbox and interrupted btrfs checkout evidence is still outstanding; the unknown-usage test expectation and ordinary-directory reconcile fixtures need correction. Registry commit `9f9b1a90` implements generation retirement and changing CAS nonces, but publication must never accept an unpinned blob after a failed pin, and the supported local-file backend needs cross-process conditional-update support. Prepublication failure cleanup, retained-pin recovery, uninstalled generation retention, and real HTTP/GC barrier tests remain review items. No finding is verified by this source review.

Source review of the follow-ups after `a0ad5e37`: R03 adds GET/POST failure tests and rejects terminating benches; real admission callback, middleware, and cache-expiry checks remain outstanding. R11 advances its watch map only after durable enqueue, including deletion events. Its review requires immutable outbox keys derived from the exact serialized payload, safe repeated acknowledgments across concurrent drainers, progress past malformed records, and independently paced backlog gauges. A relist can recover a current snapshot but cannot reconstruct every missed transition or deletion. Local-file CAS must share one canonical lock namespace, cover or reject every mutation that could bypass its comparison, and fail closed on unsupported platforms. Sequential reopen tests do not establish cross-process safety. These are review requirements for the in-progress follow-ups, not test results or finding closure.

Harness source review: the R08 query replacements cover the lifecycle callers that previously used the last 500 rows; extend restart/deadline evidence with an older open exchange. R09 must also settle terminal agent-sidebar states and include the shared module in the bench image. R10's absolute timer and single-settlement path cover the intended lifetime; the stalled-body regression is present, with no-header and aborted-response paths still to validate. R13 isolates its profile and debugging port and rebuilds source, but its fixture must select a populated thread, and test authentication must not be enabled by a production renderer query parameter. The review did not run harness tests.

Runtime evidence reported by the coordinating agent: one real bubblewrap test passed for source `a0ad5e37` in the privileged buildkit runtime, with its fixture outside masked `/tmp`. This provides scenario evidence for R02/R06; the complete required gate and final carrying source remain pending.

## Registry integrity design

Use a per-owner/digest CAS record containing a changing revision, an optional active physical key with publication pins, and retired physical keys. New physical blob keys contain a unique generation and are never reused. Existing canonical keys can be adopted as legacy generations.

1. A publisher pins each active local blob generation before checking its presence and writing the exact manifest bytes. It removes pins only after publication is known to have completed. Ambiguous failures retain protection.
2. GC captures zero-pin candidate versions before scanning authoritative manifests. It retires an unreferenced generation only with a conditional update against that captured version.
3. Every mutation changes a revision or nonce, including pin removal, so identical empty pin sets cannot revive an old ETag through ABA.
4. GC deletes only retired immutable physical keys. Reuploads install fresh generations and preserve the retirement list. A delayed delete can only affect its original generation.
5. Failed deletions remain retryable; successful cleanup conditionally removes only the retired entry it completed. Uncertain reads or malformed state retain bytes.

Implement this through one blob-access module used by GET/HEAD, mounts, monolithic and multipart uploads, publication, explicit deletion, and GC. Preserve digest validation, verbatim manifests, foreign-layer handling, and existing explicit client-delete semantics. Track unfinished upload preparation; do not collect uninstalled generations by an unsafe age-only listing. A crashed publication may retain a safe pin until recovery proves publication completed.

An expiring lock, extra scan, or touch marker does not fence an ordinary delayed object-store DELETE. Automatic recovery must rely on immutable generations. Mixed-version rollout needs a documented migration sequence: compatible readers/writers and collectors before generation state becomes authoritative. Execution of that rollout is outside this task.

## Implementation order and cleanup

Finish R01–R03/R06 and R05/R07, then R08–R10, the registry protocol, history delivery, and release/probe guarantees. R12 must include the strict R13 check before its gate is considered complete. R14 and R16 must update their real probe writers/entrypoints, not only helper scripts.

Remove duplicated security admission, exchange predicates, blob I/O, release checks, and probe ownership handling as their fixes land. Extract exchange lifecycle/delivery, tool registration, transcript projections, and history delivery where ownership gives a stable module boundary. Keep changes reviewable and behavior covered before moving large modules. Prefer self-documenting code; retain existing load-bearing constraints without adding unnecessary comments. Shell feature work remains parked.

Refresh harness architecture and recovery/runbook documentation against the resulting implementation. Mark historical object-store workspace snapshot descriptions explicitly; current workspace durability uses btrfs snapshots and replication. Record isolated recovery tests and actual limitations instead of inferring restore success from configuration.

## Verification and dependency review

- Run Rust builds, tests, clippy, dependency audits, and required integration checks in the dev pod; never run cargo on the laptop. Record source SHA, clean/dirty state, commands, versions, results, and skipped prerequisites.
- Run targeted regressions first, then locked workspace/all-target clippy and the complete relevant Rust suite on the carrying commit. Required btrfs and sandbox evidence must not be reported as passed when skipped.
- Run harness locked install, typecheck, source build, bench tests, and required renderer smoke using the deployed Node major. Use an isolated display-capable test environment and deterministic bench fixtures.
- Validate release/probe scripts with fake Kubernetes/process fixtures and failure injection, without image publication, deployment, or live mutation.
- Run Rust advisory/cargo-deny and lockfile-based harness/web dependency audits. Preserve audit time, affected dependency chains, primary advisory references, targeted remediation, and residual risks. Check reachable filesystem, authentication, command construction, and Electron boundaries alongside package results.
- The parent audit reported `extract-zip@2.0.1` through Electron 39 with GHSA-jmr9-qjv8-65gv and GHSA-7pqw-9j4j-h8q3; verify the reports and compatible remediation before accepting the suggested Electron major upgrade. This tracker does not assert a validated fix.
- Review the final diff independently, update every finding with concrete evidence or remaining work, and produce the release record before requesting any later rollout approval. No finding is closed by this initial tracker.
