# Simple operation instructions and internal editing contracts

Date: 2026-09-18
Status: Reviewed by Sol after revision to the user's minimal-instruction requirement; ready for O01
Parent: [Operation executor design](2026-09-18-harness-operation-executor-design.md)
Plan: [Flash implementation plan](../plans/2026-09-18-harness-operation-executor.md)

## 1. Main-model contract: one short instruction

The default request is:

```json
{"instruction":"In src/config.ts, change the timeout to 30000."}
```

The main model does not supply old text, a patch, revision tokens, backend argument names, or a schema-discovery call for ordinary edits. The harness injects current session/workspace scope and authorized user context. When a target is ambiguous, the executor resolves it from scoped facts or reports the specific ambiguity; it never guesses a workspace from an empty context.

The startup tool description teaches the entire common path:

> Give one bounded instruction describing the intended outcome, naming the file or resource when needed. The executor resolves tool arguments and performs the work. For edits, describe the desired change and constraints; the executor reads the source and calculates the patch. Do not repeat source text or build a patch unless its exact content is essential. Results arrive with an operation ID; inspect or cancel through this tool when needed.

Optional `inputs` and `contextRefs` carry necessary data or authorized artifact references. They are omitted when the instruction and existing context suffice. Precise code already authored by the main model may be supplied as an explicit exact artifact, avoiding another model regenerating it. This is an advanced path, not the required editing protocol.

The executable request schema rejects mixing the default instruction branch with explicit control actions. Omitted context uses trusted current-session facts and original user intent; it does not create inferred permissions or authorize a new target.

## 2. Examples

Simple edit:

```json
{"instruction":"In src/config.ts, change the timeout to 30000."}
```

Edit with a constraint:

```json
{"instruction":"In src/auth.ts, reject expired tokens before loading the user. Preserve the public API."}
```

Several bounded changes:

```json
{"instruction":"Change the timeout to 30000 in src/config.ts and update the timeout test accordingly."}
```

The last request is a dependency graph: changes to a test may depend on the selected implementation. It does not automatically authorize concurrent writes or imply that tests have passed. An instruction must contain enough intent to constrain the result; token reduction is not a reason to omit a critical requirement.

The default model-facing completion is compact:

```json
{"operationId":"op-42","state":"completed","summary":"Updated the timeout in src/config.ts.","evidenceRefs":["change-42"]}
```

Step-by-step details remain in durable operation events and the UI; they need not be repeated into the main model's context. Failed, partial, and unknown outcomes include the information necessary to choose the next action. A short completed edit response never implies build/test success.

## 3. Who calculates the edit

The internal pipeline is:

1. Resolve the target and path using trusted scope and available candidates. TypeSafe is used only for unresolved bounded semantic choices.
2. Read the required raw source internally, including relevant surrounding code. Capture a revision and resource identity.
3. Use a deterministic registered transform when its preconditions exactly match the request. Otherwise give Flash the instruction, constraints, scoped source, and bounded output contract.
4. Flash proposes content or structured replacements. It has no direct file-write authority. Missing facts trigger reads, not fabricated values.
5. Code applies the proposed replacements to an in-memory source copy and calculates the actual diff. The main model is not responsible for generating that diff.
6. Validate the proposed change, its scope, and any declared checks. Semantic assessment may flag a mismatch but cannot prove the code correct.
7. Present the concrete diff through existing approval policy when required. Revalidate the revision before applying exactly the approved change.
8. Commit through the governed workspace adapter and return observed results.

Before any provider call, apply trusted tenant/repository data policy to the exact proposed input. Reading a file successfully does not authorize sending it to Flash or Jev. Source must be explicitly eligible for the configured provider; reject unknown/disallowed classifications and exclude credential files, private keys, tokens, secret environment values, and secret-bearing excerpts. Deterministic checks are defense in depth, not proof that arbitrary source contains no secret. Do not rely on model redaction or a model's claim that an excerpt is safe. If sufficient eligible source cannot be supplied, use an applicable deterministic transform without provider exposure or return `unsupported/needs_input` without writing. A model-supplied instruction cannot override the trusted data policy. Apply the same boundary to retries, logs, and model traces.

A complicated change may exceed the bounded recipe/generation budget. Return the unresolved decision and useful evidence to the main model instead of creating an unrestricted inner coding agent. Exact content supplied by the main model bypasses regeneration but not validation or approval.

## 4. Internal editing data contract

The internal adapter still requires precise data; it is not part of the default main-model prompt:

```ts
type InternalEdit = {
  targetRef: string;
  path: string;
  expectedRevision: string;
  edits: Array<{ oldText: string; newText: string }>;
};
```

The executor obtains `targetRef` and `expectedRevision`. The generator/transform supplies replacements. The dispatcher enforces:

- Existing permitted UTF-8 text file; relative confined path; no lossy decoding.
- 1–64 ordered replacements; nonempty old text, empty new text allowed for deletion.
- Exact whitespace/newline matching; no implicit fuzzy matching or replace-all.
- Apply edits to a working copy in order; each old text occurs once at its turn. Reject the complete edit before writing if any replacement fails.
- No-op returns `changed: false`; no automatic full-file overwrite after a failed match.
- Revision and approval digest checks immediately before commit; preserve required file metadata and line endings.
- Single-file atomic replacement, governed writer serialization, truthful recovery after uncertain commits, and no claim of cross-file transactionality.

Proposed configurable initial bounds: 1 MiB text file, 64 KiB inline generated replacement payload. Oversized work uses an explicitly supported artifact path or returns to the main model; do not truncate. Internally generated content, raw file data, and diffs use authorized bounded artifact storage where needed.

Hash checks and rename alone cannot guarantee compare-and-swap against arbitrary external writers. Audit cooperating writers and document any residual race. Enable guarded edits first in isolated agent trees or an enforced cooperative writer scope. Existing workspace read emits numbered text and edit lacks the proposed revision protocol: implement raw reads and guarded writes at the tool-server boundary before enabling this recipe. A TypeScript pre-read plus an unrelated blind write is insufficient.

For paged internal reads, preserve exact raw UTF-8 text without inserted line numbers. Pages share one bounded immutable read snapshot with scoped cursors and expiry; a revision binds full original bytes, target, and path. A file changing while approval is pending requires a new proposal; do not invisibly rebase an approved patch.

## 5. Errors and bounded repair

Stable internal errors include `revision_conflict`, `snapshot_expired`, `no_match`, `ambiguous_match`, `invalid_args`, `payload_too_large`, `scope_denied`, and `outcome_unknown`. A repair loop may re-read and propose a corrected edit only within the operation budget and original intent. Any changed proposal goes through approval policy again. An unknown commit is reconciled rather than replayed.

Most internal format errors should not require the main model to learn a patch protocol. Return to it only when bounded repair fails, intent is unclear, capability is unsupported, or a decision belongs to the main agent/user. Include the exact missing information and preserve partial effects.

## 6. Capability descriptions remain available on demand

`operate({action:"describe", capability:"file.edit"})` returns a concise instruction guide, limitations, and examples by default. It is optional and never a prerequisite for a normal edit. An optional `detail:"schema"` returns the precise versioned schema for advanced exact calls. Without a capability name, return a bounded permitted index; a cursor pages that index.

Descriptions are deterministic registry lookups: no Jev/Flash call, no operation, no extra tool activation. They derive from executable contracts. The executor obtains full internal schemas automatically; it does not send every backend schema to the main model. Version-pinned advanced exact calls reject stale contracts rather than silently remapping them.

Each internal descriptor includes capability/version, summary/effect, input/output schemas, semantic rules, scope, limits, retry/conflict behavior, examples, and structured errors. O02 defines these for every enabled capability. The main model-facing interface stays the same when internal file, process, package, or environment schemas evolve.

## 7. Token and quality acceptance

- The one-field instruction above must execute without asking the main model for old/new text, patches, revision tokens, or ordinary schema discovery.
- Observe that source reads, Flash generation, diff calculation, and validation happen internally; the main model receives compact results and evidence handles.
- Measure main-agent tokens, executor-model tokens, total cost, latency, repair attempts, and successful edits separately. Fewer main-model tokens do not prove lower total cost.
- Exact supplied content is preserved and not regenerated. Simple resolved operations avoid unnecessary model calls.
- Test unclear instructions, similarly named files, semantic mismatch, source changes during approval, CRLF/Unicode, zero/multiple matches, ordered edits, no-op, oversized input, scope checks, metadata preservation, and uncertain commits.
- Test secret-bearing and unclassified source: the provider must receive none of the blocked content, and no generated write may occur. Verify approved deterministic fallback separately.
- Read-only pilot still has no runtime generation dependency. Enabling instruction-driven edits requires O04 Flash generation plus O11 scoped workspace execution and edit safeguards.

This supersedes the earlier draft that required the main model to discover and provide explicit replacement blocks for routine edits.
