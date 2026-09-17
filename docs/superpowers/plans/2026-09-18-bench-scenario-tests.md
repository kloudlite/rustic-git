# Bench scenario tests — a developer's day, driven through the bench API

Owner (2026-09-18 05:30 IST): test extensively, directly against the APIs; create sessions,
workspaces, environments, services — anything. Each finding is reported as **issue + fix plan**
(file, change, test), never a bare failure.

Ground rules for the tester: run every request inside the bench pod so no token leaves it; never
read `.bench/pi/`; keep the person's existing sessions and the `backend`/`frontend` workspaces
untouched (create your own, prefix `t-`); tear everything you created down at the end through the
same API (a delete that needs the person's card is left and reported); cap model turns at ~120.
Evidence for every step: route + status + the decisive line from the session file, exchanges,
tasks or `/v1`.

Judged on every step, not only where named: **queues** agree with `/exchanges`; **tool calls** on
the bench are only ask/ask_close/report/plan/skill/tool_search/memory/architecture/question/`kl_*`;
tool_search ≤ 1 per turn after the first; progress polls ≤ 1 per ask; no `http`, `/v1`,
`127.0.0.1`, `/opt/harness`, `pi` in any text the model sees or says; **division of work** — bench
holds semantics, workspace holds implementation; replies shaped (outcome / contracts: / changed /
next), ≤ 8 lines; the person's words forwarded, never rewritten into tool names.

## 1. First hour: onboarding

1.1 New bench session; no model set → default triple applied; footer state via `/sessions`.
1.2 "what can you do?" → identity answer, no tool calls, no internals named.
1.3 "list my workspaces" → `kl_workspaces` once; bench never listed.
1.4 "install jq" → refused: name a workspace. "install jq in frontend" → proposal → No.
1.5 "/model", "/clear", "/help" typed as text → no echo row, nothing to pi.
1.6 Two prompts 1 s apart on a cold child → order kept; QUEUED visible in `/bootstrap`.
1.7 Model switch mid-session → next turn on the new model (session file `model_change`).

## 2. Workspaces: the whole lifecycle

2.1 "create a workspace t-go with go and gnumake" → proposal → Yes → `/v1` Running → the tool
    answers when `ready`, not later; a workspace session exists; sidebar row.
2.2 "create t-bad with rust" → 422 from `/v1` surfaces as a plain sentence naming `rustc`.
2.3 "what's in t-go?" → info ask; shaped ≤ 8-line reply; no paths.
2.4 Ask t-go "add a hello program and a Makefile target run; do not build" → progress report first,
    then done; ≤ 1 poll; queue rows agree.
2.5 Same ask again while open → joins.
2.6 "stop t-go" → proposal → Yes → Stopped; a queued ask to a stopped workspace → blocked at once
    (§3.9 rule 8), not running forever.
2.7 "start t-go" → Ready again; its session resumes with history.
2.8 "clone t-go as t-go-2" → new workspace, `based_on` in the reply; "delete t-go-2" → card → gone.
2.9 Packages: "add jq to t-go" → proposal → PackagesReady turns Built; "remove jq" likewise;
    "add nodejs" (bare, no version) → 422 nearest attributes, plain sentence.
2.10 Snapshots: "push t-go with message 'first'" → snapshot listed; "restore t-go to that snapshot
     as t-go-r" → new workspace; delete it.

## 3. Environments and services

3.1 "create an environment t-env with mongodb and redis" → proposal → services Ready in `/v1`.
3.2 "attach t-go to t-env" → attached; from t-go's session "can you reach mongodb?" → the
    workspace runs a TCP check (exec card in build mode) and answers.
3.3 "intercept mongodb in t-env with t-go on 27017→27017" → proposal → intercept in force
    (`status.intercepted_by`); "release it" → released.
3.4 "add nats to t-env" → PATCH services, mongodb's data path untouched (read back the spec).
3.5 "switch my space environment to t-env" / "clear it" → `/v1/me/environments` reflects.
3.6 "delete t-env" while t-go is attached → refused with the reason, or detaches first — report
    which and whether the model explained it.

## 4. Building, images, repos

4.1 From the bench: "build and push t-go as t-go:0.1" → an ASK to t-go (never a bench exec);
    t-go reports progress, builds once, pushes, replies with the tag; `kl_images` from the bench
    lists it (registry read, no machine).
4.2 "list my repos" → `/v1/repos` via the bench token; "create repo t-repo" → proposal → exists;
    "push t-go's code to t-repo" → the workspace does it; delete t-repo at the end.
4.3 The watch loop: a long build with a `watch` → each matched line said once; the model does not
    restart the build on wake.

## 5. Agents and trees (slice 3, only if the bench on the fleet carries Task 14 — else report skip)

5.1 From t-go's session: "spawn an agent to add unit tests on a branch" → `/v1 …/trees` → tree
    ready; agent session nested under t-go; agent's ide calls carry `tree`; its `exec` in the tree
    cannot read `../main` (400); its `PORT` is in its block.
5.2 Agent pushes a branch, reports done; tree and session stay; "close the agent" → card → tree
    deleted.
5.3 Two agents at once → separate trees, separate port blocks.

## 6. Many sessions, one person

6.1 Three bench sessions open; an ask from each to t-go → t-go's inbox holds them in order; replies
    route to the right session; each Queue tab shows only its own.
6.2 A workspace session and a bench session both ask t-go's session for something → the person's
    (workspace-tab) prompt is not reordered behind a bench ask.
6.3 Archive a bench session with an open ask → the ask is cancelled, t-go is told, nothing leaks.

## 7. Failure injection

7.1 Recreate the bench pod mid-ask → after restart the open ask is re-checked (§3.9 rule 1), not
    forgotten; the reply still lands.
7.2 Stop t-go's pod (via `/v1 stop`) while its session runs a process → process row `lost`, the
    session told once; watches end.
7.3 Model refused (temporarily remove the key? NO — do not touch keys; instead pick a model id
    that does not exist for one session) → the turn's error is a footer status, the ask is
    `blocked` with a plain sentence.
7.4 A tool server that does not answer (ask a stopped workspace) → "did not answer" sanitized, no
    address.
7.5 `/v1` 5xx (cannot inject; skip) — but a 409 (quota: create 9 trees) → plain sentence naming
    the limit.

## 8. Robustness of the API itself

8.1 3 000-char prompt; whitespace-only prompt; 200 rapid prompts to one session (rate/ordering);
    unknown ids on every route → 4xx with `{error}`; malformed JSON body → 400.
8.2 `/events` WebSocket: server pongs; a silent client is terminated after ~60 s; reconnect
    replays nothing twice.
8.3 Concurrency: two `POST /proposals/{id}` for one card → second is 409.
8.4 `/fs/*`: absolute path 400, `../` 400, binary file, `/fs/log`, `/fs/changes` clean tree,
    ETag/304, a tree that does not exist 400.

## 9. Transcript and memory hygiene (read the session files)

9.1 No user row for card answers, slash lines or `/proc-stop`; dividers only for model change,
    compaction, interruption.
9.2 Memory: "remember that I prefer pnpm" → a memory file; a new session knows it.
9.3 Architecture doc: after 3.x the bench's `kl_architecture` mentions t-env and the intercept.
9.4 Plans: the harness moved items to doing/done on asks without the model writing them.

## Report

`scratchpad/api-test-report.md`: per step PASS / FAIL / SKIP(reason) with the evidence line; then
the defect list ranked by severity, each with **repro**, **suspected file**, **fix plan** (what
changes, the test that would hold it), and which implementer (harness / Rust) owns it. Under 400
lines. Then hand back with a 15-line summary.

## 10. Talking to a workspace session directly (owner, 05:45 IST)

10.1 Open the session; "what is this project?" → answers from its own tree; no ask, no platform tool.
10.2 "add a GET /health handler" → edit card (build mode); decline → nothing written; accept →
     written; plan moved by the harness; the reply names files — correct here (the trim is for
     bench-bound replies only).
10.3 "run the tests" → exec card → process; "run the server in the background on 8080" → detached
     process with a readable title in `/procs`; "stop it" → stopped, event card, no prompt echo.
10.4 "install ripgrep here" → proposal on this workspace's packages, no name needed; "switch this
     workspace to environment t-env" → own space; "create another workspace" → redirected: not
     this session's job.
10.5 Two person prompts + one bench ask arriving together → the person's order kept; the ask never
     ahead of the person's first.
10.6 Prompt mid-turn → steer vs queue; `{type:"abort"}` → "Interrupted" divider only.
10.7 "spawn an agent to add tests" → today's clone path (report; SKIP for trees until Task 14).
10.8 Hygiene: no `pi`, `/opt/harness`, absolute workspace paths in what it says; tree-relative only.
10.9 Recreate the bench pod while a detached server runs → process survives, session resumes with
     history, `/procs` still lists it.
