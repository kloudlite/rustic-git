# Harness operation executor — Sol review

Date: 2026-09-18
Reviewer: `gpt-5.6-sol`, high reasoning, agent `/root/operation_spec_review`
Scope: Design and implementation plan; no implementation or deployment review

- [Specification](2026-09-18-harness-operation-executor-design.md)
- [Flash implementation plan](../plans/2026-09-18-harness-operation-executor.md)

## Outcome

Sol reported no remaining P0 or P1 findings after revisions. The documents are ready for Flash to start O01. Sol must review the frozen executable contracts before dependent implementation begins. This result does not claim implemented behavior, successful runtime tests, or deployment readiness.

## Findings resolved

| Priority | Finding | Resolution |
| --- | --- | --- |
| P1 | Model-visible resume could be interpreted as supplying user approval | Separate recorded user/policy decisions from additional input. Only authenticated UI/trusted policy records grant or denial; bind actor, payload digest, revision, scope, and expiry. |
| P1 | Disabling the only public tool could remove controls for active operations | Keep authenticated inspection/events/cancel/decision endpoints and desktop controls independent of feature mode; define owner control after session archival. |
| P1 | Remembered discovery and plan-mode activation could restore hidden tools | One authoritative allowlist governs registration, startup, all activation paths, remembered tools, listings, and invocation dispatch. |
| P1 | Extracting raw handlers could bypass checks in extension wrappers | Both old and new paths use one policy-bearing dispatch adapter, with parity tests covering real approval and scope enforcement. |
| P1 | Flash runtime generation unnecessarily blocked the read-only pilot | O04 becomes optional for read recipes and exact supplied content; required only for explicit generated-content capabilities. |
| P1 | Process inspection could hide an unrestricted workspace-agent call | Pilot uses bench-owned process metadata. Later workspace logs/actions require scoped deterministic transport, with tests proving no hidden LLM session. |

## Completeness check

The primary agent compared the specification against the preceding discussion and added section 13 mapping each requirement to a design section and implementation task. All discussed opportunities are represented; later delivery stages are named explicitly. Sol independently found the visible architecture and opportunity map complete within its review context.

## Remaining implementation gates

- O01: executable schemas, full transition table, typed output-binding syntax, authenticated turn identity, and fixtures.
- O02: reproduce and fix tool-contract drift against the actual integration SHA.
- O10: measured task-specific thresholds and latency/cost/accuracy evidence.
- O11/O12: per-capability and per-extension acceptance before enablement.

## Follow-up: minimal editing instructions

The user subsequently required simple editing instructions with patches calculated inside the executor. The [capability protocol](2026-09-18-harness-capability-contracts.md) replaces a draft requiring main-model old/new blocks with `operate({instruction: ...})`, internal source reads and diff generation, optional schema discovery, and compact results. Parent interface and O01/O04/O11 tasks were updated. Sol reviewed the delta and confirmed no remaining blocking contradiction or interface issue after the edit example was aligned and a trusted provider-input policy was added for source/secret handling. Secret-bearing source fixtures are required. Ready for O01; instruction-driven guarded edits remain gated on O04 and O11.

## Document verification

Local Markdown links and fenced-block balance were checked. All 13 task IDs are present exactly once. No production or application code was changed for this planning task. Existing unrelated working-tree changes were left untouched.
