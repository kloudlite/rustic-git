# Operation Executor O06/O07 Wave Design

Date: 2026-09-19
Baseline: `feature/operation-executor` at `927633b2`

## Scope

Implement the two now-unblocked slices in parallel:

- O06 owns DAG validation, scheduling, execution, conflicts, cancellation, and result aggregation.
- O07 owns deterministic candidate resolution, bounded recipes, argument provenance, and recipe expansion limits.

O08 remains out of scope until both slices pass focused review and integrated verification.

## Ownership

O06 creates `harness/bench/src/operations/scheduler.ts`, `executor.ts`, and focused tests. It executes only through the accepted O02 capability interfaces and persists lifecycle changes through O05 contracts.

O07 creates `harness/bench/src/operations/resolve.ts`, `recipes.ts`, and focused tests. It may consume O01/O02/O03 interfaces but must not change their contracts. Initial recipes remain read-only and cannot issue an open-ended workspace-agent ask.

Shared package scripts or typecheck configuration are coordinator-owned. Agents must request shared-file changes rather than editing overlapping integration files.

## Execution And Integration

Both agents branch from the exact baseline and use strict test-driven development. Tests and builds run only in the authoritative dev pod. Each agent commits its branch but does not merge it.

After independent final-source review, integrate O06 first and O07 second. Run focused suites after each merge, then the complete non-renderer suite, combined typecheck, renderer build, and Electron/Xvfb boot test at the integrated SHA. O08 starts only after that gate passes.

## Acceptance

O06 must prove independent reads overlap, dependencies block correctly, conflicting resources never overlap, fairness is bounded, cancellation propagates, unknown effects reconcile before retry, and terminal aggregation reports exact partial outcomes.

O07 must prove literal and ID resolution precede semantic selection; duplicate, absent, stale, typo, negated, and cross-tenant candidates fail safely; dependent choices use staged current facts; provenance preserves known, unspecified, explicit-clear, ambiguous, and unsupported states; and recipe limits return unsupported rather than expanding without bound.
