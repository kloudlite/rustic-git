# Operation UI remediation and production bridge

Date: 2026-09-19
Status: Implemented as the O09 projection/control slice; production operation production remains blocked on O05/O08/deployment, and final authoritative broad-run verification is pending
Scope: O09 correctness, production visibility integration, accessibility, and verification

## Goal

Make the operation UI a truthful, accessible projection of frozen O01 records and events, mount it in the production renderer, expose it in the taskboard, and connect its controls through a credential-free renderer bridge. The slice must reject contradictory lifecycle input rather than normalize it into a plausible state.

## Lifecycle projection

The renderer mirrors O01's complete operation and step transition tables. Event phases are observations that select an O01 transition only when the current state, trigger, and required evidence agree. Missing event-envelope facts, including dependency skips and reconciliation conclusions, require an authoritative snapshot instead of an inferred transition.

Snapshots and events are runtime-validated at ingress. Revisions never regress. Duplicate sequence numbers are idempotent only when their canonical payloads agree; conflicting duplicates trigger snapshot repair. A repair snapshot may advance the cursor, but held events newer than that cursor remain replayable. The view retains authoritative unknown outcomes and retry metadata needed to distinguish legal idempotent retries from prohibited redispatch.

Cancellation intent remains separate from durable operation state until the trusted control accepts it. Decision presentations must match an open decision's operation, step, class, revision, and expiry before controls become answerable. Terminal events require the same evidence and no-open-work invariants as terminal snapshots.

## Production bridge

The bench exposes narrow operation-control routes for snapshot inspection, cursor event replay, cancellation, and decisions. Routes validate all O01 payloads and authorize the current owner through the existing person-only bench admission boundary. Operation controls remain independent of model/provider availability.

The renderer never receives a bearer credential. It sends operation intents over the desktop preload/main boundary; the desktop main process attaches its existing login credential when calling the bench. Failed, stale, and unauthorized controls return typed errors to the renderer.

The renderer owns one operation projection per `operate` tool call. It loads the snapshot before replaying events, services resync requests, and mounts the operation panel below the tool-call row. Taskboard operation rows are derived from the same projections, so task state and operation state cannot diverge independently.

The bridge is intentionally limited to visibility and control. It does not implement O05 scheduling, capability dispatch, provider calls, or persistence internals that are outside O09. It consumes frozen O01 snapshots/events from the operation source and provides a concrete production seam for the later O05 implementation.

## UI and accessibility

Panel and step disclosures use native buttons with controlled expansion, `aria-expanded`, and `aria-controls`. Lifecycle and repair changes have status/live-region semantics. Additional input uses a labelled form and submits with Enter.

State-changing controls expose pending and error states and disable duplicate submissions. The confirmation overlay is a labelled modal dialog with initial focus, Escape handling, focus containment, and focus restoration. Layout wraps at narrow widths, and full evidence identifiers remain inspectable.

## Tests

Reducer regressions cover every frozen O01 transition, revision regression, duplicate conflicts, snapshot/held-event interleaving, guarded retry, cancellation refusal, decision linkage, unknown outcomes, and terminal evidence.

Fixture tests use independent semantic assertions and truthful requests/results. Component tests exercise keyboard disclosure, controlled expansion, dialog focus, labelled input, live status, duplicate submission prevention, and narrow layout. Integration tests cover renderer mounting, taskboard projection, authenticated inspect/events/cancel/decision controls, reconnect, stale revisions, and cross-owner refusal.

Authoritative verification runs against an exact temporary copy in the dev pod with Node 22: typecheck, targeted operation tests, full bench tests, renderer build, and renderer boot. Temporary copies are removed afterward.
