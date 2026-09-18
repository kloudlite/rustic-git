# Operation Evaluation Isolation Design

Date: 2026-09-19
Task: O10 evaluation hardening
Base SHA: `d2944fcb695cba15e9ec2809672e279a8991bc45`

## Goal

Evaluate the frozen baseline, current heuristic, and proposed TypeSafeAI JEV subject on the same sanitized cases without exposing held-out answers to any subject and without making operation dispatch possible.

## Chosen Approach

Use a pure in-process evaluator with a reviewer-only oracle bundle injected at runtime. The committed corpus contains public case inputs and tuning expectations. Held-out cases carry only stable case identifiers and public inputs; their expectations, forbidden outcomes, and fault expectations live in an untracked reviewer fixture or CI secret.

This keeps the evaluator deterministic and easy to test while making held-out custody an operational boundary rather than an import convention. A separate grading service would provide stronger process isolation, but adds protocol, deployment, and failure complexity that O10 does not need. A committed test-only oracle is rejected because code under evaluation could still inspect it.

## Inputs

The public corpus defines:

- corpus and contract versions;
- synthetic case inputs, scopes, candidates, and split assignment;
- tuning expectations and safety constraints;
- held-out case identifiers with no oracle-bearing fields;
- declared capability effects.

The reviewer oracle bundle defines:

- the matching corpus version and oracle contract version;
- exactly one oracle for every selected held-out case;
- expected calls or expected refusal;
- forbidden capabilities and targets;
- deferred and provider-fault classifications where applicable.

The loader fails before running a subject when either input is malformed, versions disagree, an oracle is missing or duplicated, an unknown case is named, or a selected held-out case has no oracle. Oracle data is joined only inside the scorer and is never included in subject input.

## Subject Isolation

An evaluation suite names exactly three distinct roles:

- `baseline`: the frozen literal deterministic implementation;
- `current`: the production heuristic under comparison;
- `proposed`: the TypeSafeAI JEV implementation using the established OpenRouter-backed provider configuration.

Each role receives the same immutable snapshot of a case. Subject IDs and role assignments must be unique. The runner rejects incomplete suites and never substitutes one implementation for another.

Subjects receive only authorized intent, scope, candidates, case identity needed for deterministic fixture routing, and an evaluation context. They never receive expectations, forbidden outcomes, split labels, fault expectations, or the reviewer bundle.

The evaluation context exposes timing, cancellation, and injected provider-fault controls. It exposes no executor, capability registry, workspace client, shell, or operation dispatch function. A dispatch tripwire is retained only as a test boundary: any attempted use becomes a recorded safety violation and fails the case rather than executing work.

## Proposed Provider Path

The proposed subject adapts each eligible case into bounded TypeSafeAI Choice judgments. It reuses the existing provider-independent judgment contracts and TypeSafe adapter behavior instead of introducing another provider abstraction. Evaluation mode is always `shadow`, and the resulting judgment remains advisory.

Provider credentials come from the existing trusted OpenRouter/TypeSafeAI configuration path. They are never placed in corpus data, subject input, reports, exceptions, or snapshots. Missing credentials produce a classified provider failure. The adapter's existing request bounds, response bounds, model pin, timeout, retry, cancellation, budget, input-policy, and redaction rules remain authoritative.

## Fault Handling

Fault injection is deterministic and case-scoped. Supported classes are timeout, caller abort, malformed response, provider error, missing credentials, exhausted budget, and attempted dispatch. Each class maps to a stable report code and cannot produce a selected operation.

Thrown subject errors are converted to sanitized failures. Provider response bodies, arbitrary transport messages, authorization headers, and credential values are never copied into the report. Fault injection does not require a live provider call.

## Scoring And Reports

All three roles are evaluated against the same selected case IDs and immutable snapshots. Cohort mismatch is a hard error.

Per-case reporting includes:

- action, argument, dependency, outcome, and whole-call correctness;
- abstention and unnecessary/missed abstention;
- candidate misses, stale targets, and unsafe proposals;
- dispatch attempts and safety-violation codes;
- classified fault and parse outcome;
- measured latency;
- provider attempts, model/version provenance, and token usage;
- cost only when usage and validated pricing are complete.

Per-role and per-split summaries retain null for unknown rates or costs. Comparisons report baseline-to-current and baseline-to-proposed deltas over identical cohorts. Thresholds remain advisory until reviewer approval.

## Testing

Tests first establish the boundary:

- the public held-out fixture contains no expectations or forbidden outcomes;
- held-out evaluation cannot start without a matching reviewer bundle;
- malformed, duplicate, missing, extra, or version-mismatched oracles fail closed;
- subjects cannot observe oracle-bearing fields;
- baseline, current, and proposed roles are distinct and complete;
- no dispatch capability is reachable, and a tripwire attempt is reported;
- every fault class is deterministic and never becomes a selection;
- metric denominators, latency percentiles, usage, cost, cohort comparisons, and redaction are correct;
- the TypeSafeAI subject maps Choice and provider failures into evaluation attempts without authorizing work.

Focused Node tests and a strict TypeScript lane run in a disposable checkout under `/work/src` in the dev pod. Existing O01 and TypeSafeAI adapter tests run as regression lanes. Cargo is not run on the Mac.

## Repository Hygiene

The O03 sibling worktree is mid-conflict and is reference-only. Its files are not merged wholesale. O03 integration must come from a clean commit or be copied as the smallest reviewed dependency set.

Unrelated `.claude/settings.json` and `.claude/skills/graft/SKILL.md` worktree changes are excluded from this task and restored only with explicit care not to overwrite concurrent user work.
