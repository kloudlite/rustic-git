# O02 Audit Remediation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace O02's synthetic raw-handler registry with trusted typed adapters, validated recorded decisions, exact argument semantics, structured outputs/errors, and comprehensive parity tests.

**Architecture:** Capability definitions remain the reviewed source of schemas and policy metadata. Trusted integration constructs an opaque runtime of shared adapters; the registry validates and dispatches through that runtime, while legacy Pi tools render the same typed outcomes. O05/O08 will persist and create decisions and register `operate`, but O02 validates every decision before effects.

**Tech Stack:** TypeScript, Node test runner, TypeBox/Pi extension adapters, existing O01 operation contracts.

---

### Task 1: Compile the complete operation surface

**Files:**
- Create: `harness/tsconfig.operations.json`
- Modify: `harness/package.json`
- Modify: `harness/pi/kloudlite.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`

1. Add a source-level test proving `lookAround` resolves a workspace name through the shared resolver.
2. Run the targeted test in the dev pod and confirm the undefined resolver path fails.
3. Move or reuse `resolveNamed` so both `lookAround` and bench tools call one function.
4. Add a NodeNext no-emit config covering `bench/src/operations/**/*.ts`, `pi/kloudlite.ts`, `pi/catalog.ts`, and `pi/workspace-tools.ts`, with TypeScript-extension imports enabled.
5. Add it to `npm run typecheck` and run the complete typecheck in the dev pod.

### Task 2: Preserve exact argument intent

**Files:**
- Modify: `harness/bench/src/operations/arguments.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/pi/kloudlite.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`

1. Add failing tests showing omitted intercept workspace is not clear, explicit null is clear, and irrelevant process fields are rejected.
2. Extend validated arguments to carry states through dispatch rather than deleting clear intent.
3. Make intercept clear require explicit null and have its adapter issue DELETE only for `explicitly_clear`.
4. Extend process action metadata with allowed fields and report `mixed_action` for irrelevant fields.
5. Run the targeted tests.

### Task 3: Introduce typed capability outcomes

**Files:**
- Modify: `harness/pi/kloudlite.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`

1. Replace the generic-output tests with capability-specific schema assertions for workspace rows/documents, skill text, progress, process rows, environment state, package state and service state.
2. Define JSON-safe structured success/error results and a legacy renderer to `ToolResult`.
3. Change dispatch policy to preserve explicit `OperationErrorCode` values rather than converting all failures to `execution_failure`.
4. Map known HTTP/resolution failures into the frozen taxonomy and preserve `unknown_outcome` for uncertain mutations.
5. Run output-binding and error-mapping tests.

### Task 4: Build the trusted runtime and bench process adapter

**Files:**
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/bench/src/ledger.ts`
- Modify: `harness/bench/src/bench.ts`
- Modify: `harness/pi/kloudlite.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`
- Test: `harness/bench/test/ledger.test.ts`

1. Add failing tests proving arbitrary handler maps are impossible, every enabled capability has a trusted adapter, and process listing filters/limits structured rows.
2. Replace `CapabilityDispatchDeps.tools` with an opaque `CapabilityRuntime` whose construction is restricted to trusted adapter factories.
3. Extract shared platform adapter functions used by both Pi registration and the runtime.
4. Add a process metadata adapter over `Procs.all()` with `includeEnded`, workspace filtering, deterministic ordering and the descriptor row limit.
5. Wire the runtime from the bench integration context without registering `operate`.
6. Run registry, ledger and legacy parity tests.

### Task 5: Require validated recorded decisions

**Files:**
- Modify: `harness/bench/src/operations/capabilities.ts`
- Modify: `harness/pi/kloudlite.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`
- Test: `harness/bench/test/operations-contracts.test.ts`

1. Add failing tests for forged actor/session/step/digest/revision/policy/expiry, denial, replay and a valid grant.
2. Define trusted dispatch context and pending decision expectation inputs sourced outside model arguments.
3. Change approval acquisition to return `RecordedDecision` and validate it with `checkResumeAgainstRecord` immediately before dispatch.
4. Preserve the legacy proposal bridge by adapting its authenticated answer into the trusted decision interface at the outer registration boundary; do not persist it in O02.
5. Run approval and legacy proposal tests.

### Task 6: Prove mutation and collection parity

**Files:**
- Modify: `harness/pi/kloudlite.ts`
- Modify: `harness/bench/src/operations/capabilities.ts`
- Test: `harness/bench/test/operations-capabilities.test.ts`
- Test: `harness/bench/test/bench-tools.test.ts`

1. Add capability-dispatch tests for service add/remove and package add/remove preserving unrelated rows exactly.
2. Exercise disabled mutations through an explicitly constructed test runtime and a valid recorded decision, without adding them to the pilot allowlist.
3. Verify restore, create-source, scope and unknown-outcome paths through the same runtime.
4. Add an enabled-read parity matrix covering success, refusal and structured failures for every initial read capability.
5. Run the complete targeted suite.

### Task 7: Authoritative verification and cleanup

**Files:**
- Verify all changed files.

1. Reconstruct the exact dirty O02 worktree in a disposable `/work/review-o02` dev-pod worktree.
2. Record HEAD and `git status --short --untracked-files=all`.
3. Run `git diff --check`.
4. Run `npm run typecheck` under Node 22 with the pod's installed dependencies.
5. Run targeted O02/legacy tests and the full bench suite; use Xvfb for renderer boot.
6. Remove the dependency symlink, temporary worktree, patch and archive, then assert every path is absent.
7. Report changed files, exact commands/counts, and only genuine downstream blockers.
