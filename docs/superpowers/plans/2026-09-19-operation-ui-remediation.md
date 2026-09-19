# Operation UI Remediation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Correct every technically valid O09 audit finding and ship a tested production renderer/control integration.

**Architecture:** Keep O01 as the sole lifecycle authority. Add strict renderer ingress/projection, a credential-free desktop bridge to narrow authenticated bench controls, and derive both the tool-call panel and taskboard rows from one operation store.

**Tech Stack:** TypeScript, SolidJS, Electron IPC/preload, Node test runner, Vite, existing harness bench HTTP server and desktop client.

**Status (2026-09-19):** Tasks 1–7 are implemented and covered by focused lifecycle, component,
store/mount/taskboard, control-route, desktop bridge, online-authorizer, and configured-source-seam
tests. Task 8 documentation is updated. The plan's approved intent is unchanged. O05's durable
`OperationSource`, O08's `operate` registration/executor/result-production emission, and deployment
`KL_OPERATION_CONTROL_MODULE` configuration remain outside this plan and absent, so there is no live
end-to-end production operation. Final authoritative verification is still open because the renderer
boot broad-run environment issue remains unresolved; no clean full-suite claim is made.

---

### Task 1: Freeze lifecycle regressions

**Files:**
- Modify: `harness/bench/test/operations-ui.test.ts`
- Modify: `harness/src/renderer/operations/fixtures/scenarios.ts`

1. Add failing tests for revision regression, conflicting duplicate sequences, repair-snapshot interleaving, first-seen dispatch, complete O01 transition coverage, guarded retries, decision linkage, explicit unknown outcomes, cancellation rejection, and terminal evidence.
2. Run `node --test bench/test/operations-ui.test.ts` in an exact temporary dev-pod copy and record the expected failures.
3. Correct fixture request/result truth independently of reducer behavior.
4. Re-run and retain failures that require production changes.

### Task 2: Make the reducer an exact O01 projection

**Files:**
- Modify: `harness/src/renderer/operations/types.ts`
- Modify: `harness/src/renderer/operations/reduce.ts`
- Modify: `harness/src/renderer/operations/present.ts`
- Test: `harness/bench/test/operations-ui.test.ts`

1. Add retry metadata, authoritative unknown outcomes, control-intent state, and contradiction/resync reasons to the view.
2. Runtime-validate snapshots/events at ingress.
3. Reject regressing revisions and conflicting duplicates.
4. Preserve held events across snapshot repair and require complete cursor replay.
5. Remove same-destination transition shortcuts and first-seen dispatch creation.
6. Represent denied approval and reconciliation through exact O01 edges or snapshot repair.
7. Require legal retry metadata and terminal evidence/invariants.
8. Keep cancellation intent separate from durable state.
9. Run the targeted test until green.

### Task 3: Harden decisions and summaries

**Files:**
- Modify: `harness/src/renderer/operations/bridge.ts`
- Modify: `harness/src/renderer/operations/reduce.ts`
- Modify: `harness/src/renderer/operations/present.ts`
- Test: `harness/bench/test/operations-ui.test.ts`

1. Add failing tests for cross-operation, wrong-step/class, stale, expired, and nonpending presentations.
2. Validate presentations against pending records and lifecycle state.
3. Make callback payloads unavailable while submission is pending or state is contradictory.
4. Add explicit cost-unavailable presentation instead of claiming cost visibility.
5. Run targeted tests until green.

### Task 4: Add accessible component tests and controls

**Files:**
- Create: `harness/bench/test/operations-components.test.tsx`
- Modify: `harness/src/renderer/operations/components/OperationPanel.tsx`
- Modify: `harness/src/renderer/operations/components/StepList.tsx`
- Modify: `harness/src/renderer/operations/components/DecisionPrompt.tsx`
- Modify: `harness/src/renderer/operations/styles/operations.css`
- Modify: `harness/src/renderer/ui/Confirm.tsx`
- Modify: `harness/src/renderer/ui/Button.tsx`

1. Add a DOM test dependency only if the existing harness has no usable renderer test helper.
2. Write failing tests for controlled expansion, keyboard step disclosure, labelled input/Enter, live statuses, dialog semantics/focus/Escape/restore, full evidence access, and duplicate-submit prevention.
3. Implement native disclosure controls and accessible relationships.
4. Implement promise-aware pending/error control state.
5. Make `Confirm` a proper modal dialog without regressing existing callers.
6. Add responsive wrapping and evidence title/copy access.
7. Run component and existing renderer tests until green.

### Task 5: Add the operation renderer store and mount

**Files:**
- Create: `harness/src/renderer/operations/store.ts`
- Modify: `harness/src/renderer/operations/index.ts`
- Modify: `harness/src/renderer/components/ToolCall.tsx`
- Modify: `harness/src/renderer/live.ts`
- Test: `harness/bench/test/operations-integration.test.ts`

1. Write failing tests that an `operate` action with an operation ID mounts one panel and updates it from snapshot/events.
2. Implement one projection per operation ID with snapshot-first loading, cursor replay, reconnect, and resync.
3. Mount the panel below `operate` calls without changing other tool rendering.
4. Ensure panel lifecycle is disposed with its transcript/session.
5. Run integration tests until green.

### Task 6: Project operations into the taskboard

**Files:**
- Modify: `harness/src/renderer/live.ts`
- Modify: `harness/src/renderer/components/inspector/Tasks.tsx`
- Modify: `harness/src/renderer/components/TaskView.tsx`
- Test: `harness/bench/test/operations-integration.test.ts`

1. Write failing tests for running, waiting, reconciling, partial, failed, cancelled, and completed operation rows.
2. Derive taskboard rows from operation views rather than independent inferred state.
3. Open the same operation details from the taskboard.
4. Preserve existing process/task behavior.
5. Run taskboard and renderer tests until green.

### Task 7: Implement the production control bridge

**Files:**
- Create: `harness/bench/src/operations/control.ts`
- Modify: `harness/bench/src/server.ts`
- Modify: `harness/src/bench-client.ts`
- Modify: `harness/src/main.ts`
- Modify: `harness/src/preload.ts`
- Modify: renderer bridge/store files
- Test: `harness/bench/test/operations-control.test.ts`
- Test: `harness/bench/test/operations-integration.test.ts`

1. Write failing route tests for inspect, cursor events, cancel, decisions, malformed O01 payloads, stale revision, unauthenticated access, child-token refusal, and cross-owner refusal.
2. Define an operation-source interface so O09 does not invent O05 persistence.
3. Mount narrow authenticated routes using the existing owner admission path.
4. Add desktop IPC/preload methods; attach login credentials only in main process.
5. Wire renderer store controls to the bridge and surface typed errors.
6. Add one in-process production integration fixture using a real HTTP server and renderer store.
7. Run control and integration tests until green.

### Task 8: Update handoff and verification evidence

**Files:**
- Modify: `docs/superpowers/plans/2026-09-18-operation-ui-handoff.md`
- Modify: `harness/package.json` only if tests require a new command/dependency

1. Update file inventory, exact interfaces, remaining O05 boundary, accessibility guarantees, and truthful gate status.
2. Run `npm run typecheck` in an exact temporary dev-pod copy.
3. Run targeted operation, component, control, and integration tests.
4. Run `npm run bench:test`.
5. Run `npm run build:renderer` and the renderer boot test.
6. Confirm all temporary `/work/o09-*` directories are removed.
7. Review the complete diff against the approved design and O01 tables.
