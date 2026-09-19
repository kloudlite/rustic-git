# O02 audit remediation design

Date: 2026-09-18

## Goal

Close every technically valid O02 audit finding without taking ownership of O05 persistence or
O08 public `operate` integration. O02 supplies trusted executable capability adapters, typed
results and errors, complete argument semantics, and a decision-validation boundary that later
stages can call.

## Trusted dispatch

The executor never receives a map of raw functions. A `CapabilityRuntime` is assembled inside the
trusted bench integration context from reviewed adapters. The registry resolves only capabilities
that are enabled and present in that runtime. Legacy tools and capability dispatch call the same
adapter, so argument checks, scope checks, approval and transport cannot drift.

The runtime includes a bench-owned process-list adapter over `Procs.all()`. It returns bounded,
structured rows and never reaches a workspace tool server.

## Approval

A mutating dispatch asks a trusted approval bridge for a `RecordedDecision`, not a boolean. O02
builds the expected actor/session/operation/step/payload/revision/policy/expiry tuple and validates
the returned record through `checkResumeAgainstRecord`. A valid denial refuses the step; a valid
grant permits one dispatch. O05/O08 remain responsible for persisting records and creating them
from authenticated UI or trusted policy actions.

## Results and errors

Capability adapters return JSON-safe structured values. Each descriptor publishes a matching
capability-specific output schema so O06/O07 can bind output paths without parsing display text.
Legacy tools render structured values to their existing text envelope at their outer boundary.

Adapters classify failures into the frozen operation error taxonomy. Known HTTP and resolution
failures map to validation, no-match, ambiguity, permission, scope, conflict or provider/execution
errors. A mutation whose commit status is uncertain returns `unknown_outcome`; it is never treated
as an ordinary retryable execution failure.

## Arguments

Validated argument states remain available through dispatch. Omission stays `unspecified`.
Explicit clear remains `explicitly_clear` and is conveyed to the adapter separately from ordinary
JSON arguments. Intercept clearing therefore requires explicit `null`; omission does not silently
delete routing.

Process actions reject fields that are irrelevant to the selected action. Workspace creation
keeps exclusive sources; environment snapshot creation may still accept service overrides because
that is an explicit API feature rather than conflicting source selection.

Whole-list service and package adapters preserve unrelated rows and are tested through capability
dispatch even while mutations remain disabled in the pilot allowlist.

## Compilation and tests

A dedicated operations TypeScript configuration covers operation contracts, adapters and their Pi
dependencies and becomes part of `npm run typecheck`. The undefined workspace resolver is replaced
by the existing shared resolver.

Regression tests cover trusted runtime confinement, all enabled read adapters, bench process
metadata, complete recorded-decision validation, typed outputs, structured errors, explicit clear,
per-action process fields, whole-list preservation and legacy/capability parity. Verification runs
only in a disposable dev-pod copy of the exact dirty worktree.
