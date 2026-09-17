# Session Boundaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every session remembers its model/thinking/effort; every pod gets a home-only `ttyd` shell sidecar; no session has hands where it runs; subagents work in nested-snapshot trees served by the workspace's own tool server, each exec wrapped in bubblewrap.

**Architecture:** Three slices that ship alone, in order. Slice 1 is harness-only (bench + renderer). Slice 2 changes pod shape (Rust: pod specs, shell image, NetworkPolicy) and removes the bench's hands (harness). Slice 3 adds `Workspace.spec.trees` (Rust: CRD, `/v1`, node agent, tool server `tree` + bwrap) and moves the agent path onto trees (harness).

**Tech Stack:** Rust (axum, kube-rs, btrfs via `Command`), TypeScript (`harness-bench` Node runtime, pi RPC), Electron + Solid renderer, xterm.js, `ttyd`, `bubblewrap` from the Nix pin.

**Spec:** `docs/superpowers/specs/2026-09-17-session-boundaries-design.md`

## Global Constraints

- Every decision in the spec is the owner's unless marked *proposed*; do not re-decide.
- Commit subjects: imperative, sentence case, no tool attribution; the commit-msg hook rejects "Claude".
- Rust gate before any ship: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on the laptop (target on `/Volumes/kdisk`), never in the dev pod's shared target.
- Harness gate: `npm run typecheck`, `npm run bench:test`, `npm run build`, and the Electron+CDP boot test (`bench/test/renderer-boot.test.ts`).
- Stage by path (`git add <file>…`), never `git commit -a`: another implementer may share the worktree.
- No token, key or secret is ever logged, printed in a test name, or echoed in an error string.
- Paths a model sees are tree-relative (spec §3.5). Any new tool result, error or process title that prints a path must strip the tree root first.
- Every new `/v1` route that writes spec goes through `guard_alloc`/`may_act_on` like its siblings; the node agent writes status only.
- Slash-command dialogs render **in place of the composer**, compact, like the permission prompt; no floating modals.
- Probe ids added to `crates/workspaces/src/slo/catalogue.rs` must also land in `deploy/slo.md` (a test holds them equal) and the web fixture.

---

# Slice 1 — Model, thinking level and effort (harness only)

## File structure

- Modify `harness/bench/src/sessions.ts` — `SessionRow` gains `thinking?`, `effort?`; `create()` takes a triple.
- Create `harness/bench/src/defaults.ts` — the general default triple, `{dir}/.bench/defaults.json`, read-modify-write.
- Modify `harness/bench/src/rpc-child.ts` — `applyTriple()` sends `set_model`/`set_thinking_level`/effort after `session_start`; `models()` wraps `get_available_models`.
- Modify `harness/bench/src/bench.ts` — apply triple on child start; `POST /sessions/{id}/model` route handler; `PROVIDERS` catalogue with `wired` flag.
- Modify `harness/bench/src/server.ts` — routes `GET /models`, `POST /sessions/{id}/model`, `GET /defaults`.
- Modify `harness/bench/src/providers.ts` — every pi provider listed; `wired: boolean` (DeepSeek true).
- Modify `harness/src/renderer/live.ts` — `models()`, `setModel()`, per-thread triple in the session snapshot.
- Create `harness/src/renderer/components/ModelDialog.tsx` — `/model` dialog.
- Modify `harness/src/renderer/components/Chat.tsx` — footer segments; slash menu entry `/model`; `Ctrl+T`, `Ctrl+E`.
- Modify `harness/src/renderer/keys.ts` — `thinking: ^T`, `effort: ^E` (renderer-only, never sent to pi).
- Modify `harness/src/renderer/rows.ts` — `modeParts()` gains `thinking`, `effort`.
- Test `harness/bench/test/defaults.test.ts`, `harness/bench/test/model-routes.test.ts`, `harness/bench/test/renderer-rows.test.ts`.

## Task 1: Default triple store and session fields

**Files:**
- Create: `harness/bench/src/defaults.ts`
- Modify: `harness/bench/src/sessions.ts`
- Test: `harness/bench/test/defaults.test.ts`

**Interfaces:**
- Produces: `type Triple = { model?: string; thinking?: Thinking; effort?: Effort }`, `type Thinking = "off"|"minimal"|"low"|"medium"|"high"|"xhigh"`, `type Effort = "low"|"medium"|"high"|"max"`; `class Defaults { constructor(dir); get(): Triple; set(t: Partial<Triple>): Triple }`; `SessionRow.thinking?`, `SessionRow.effort?`; `Sessions.create(t?: Triple)`.

- [ ] **Step 1: Write the failing test**

```ts
// harness/bench/test/defaults.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Defaults } from "../src/defaults.ts";

test("defaults persist and merge", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "defaults-"));
  const d = new Defaults(dir);
  assert.deepEqual(d.get(), {});
  d.set({ model: "deepseek/deepseek-reasoner" });
  d.set({ thinking: "high" });
  assert.deepEqual(new Defaults(dir).get(), { model: "deepseek/deepseek-reasoner", thinking: "high" });
});

test("effort is dropped when set to undefined", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "defaults-"));
  const d = new Defaults(dir);
  d.set({ effort: "max" });
  d.set({ effort: undefined });
  assert.equal(d.get().effort, undefined);
});
```

- [ ] **Step 2: Run it: `cd harness && npm run bench:test -- test/defaults.test.ts` → fails, module not found.**

- [ ] **Step 3: Implement**

```ts
// harness/bench/src/defaults.ts
import fs from "node:fs";
import path from "node:path";
import { readJson, replaceJson } from "./ledger.ts";

export type Thinking = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
export type Effort = "low" | "medium" | "high" | "max";
export type Triple = { model?: string; thinking?: Thinking; effort?: Effort };
export const THINKING: Thinking[] = ["off", "minimal", "low", "medium", "high", "xhigh"];
export const EFFORT: Effort[] = ["low", "medium", "high", "max"];

/**
 * The general default: the last pick the person made ANYWHERE (spec §1.2). A dispatch that names a
 * model never writes here; only a person's pick does. One small file, read once, replaced on write.
 */
export class Defaults {
  private file: string;
  private cur: Triple;
  constructor(dir: string) {
    this.file = path.join(dir, "defaults.json");
    this.cur = readJson<Triple>(this.file, {});
  }
  get(): Triple { return { ...this.cur }; }
  set(t: Partial<Triple>): Triple {
    const next: Triple = { ...this.cur, ...t };
    for (const k of Object.keys(next) as (keyof Triple)[]) if (next[k] === undefined) delete next[k];
    this.cur = next;
    replaceJson(this.file, next);
    return this.get();
  }
}
```

`readJson`/`replaceJson` already exist in `ledger.ts`; export them if they are not.

In `sessions.ts`: add `thinking?: Thinking; effort?: Effort` to `SessionRow`; add both to `IMPORT_FIELDS` in `bench.ts`; `create(t?: Triple)` writes all three; `thread(...)` takes `t?: Triple` likewise. `update(id, patch)` already exists and is what the model route uses.

- [ ] **Step 4: Run the test → passes. Run `npm run typecheck`.**
- [ ] **Step 5: Commit** `git add harness/bench/src/defaults.ts harness/bench/src/sessions.ts harness/bench/src/bench.ts harness/bench/test/defaults.test.ts && git commit -m "Sessions remember model, thinking and effort; the bench keeps a default triple"`

## Task 2: Apply the triple to pi and expose models

**Files:**
- Modify: `harness/bench/src/rpc-child.ts`, `harness/bench/src/bench.ts`, `harness/bench/src/server.ts`, `harness/bench/src/providers.ts`
- Test: `harness/bench/test/model-routes.test.ts` (uses `test/fake-pi.ts`)

**Interfaces:**
- Consumes: `Defaults`, `Triple` from Task 1.
- Produces: `RpcChild.applyTriple(t: Triple): Promise<void>` (sends `set_model {provider, modelId}`, `set_thinking_level {level}`, and effort via `set_model`'s `options.effort` when the model's `capabilities.effort` is true — check the locked pi's `rpc.md`; if effort is not a pi option, store it and pass it as `--effort` env `PI_EFFORT` the extension reads, and note it); `RpcChild.models(): Promise<ModelInfo[]>`; routes `GET /models` → `{providers: [{id,label,wired,models:[{id,name,thinking:boolean,effort:boolean}]}]}`, `POST /sessions/{id}/model {model?,thinking?,effort?, default?: boolean}` → session row, `GET /defaults` → Triple.

- [ ] **Step 1: Test**

```ts
// harness/bench/test/model-routes.test.ts
test("a person's pick moves the default; a dispatch's pick does not", async () => {
  const b = await startBench();              // helper from bench-routes.test.ts
  const a = await post(b, "/sessions", {});
  await post(b, `/sessions/${a.id}/model`, { model: "deepseek/deepseek-chat", thinking: "low" });
  const c = await post(b, "/sessions", {});
  assert.equal(c.model, "deepseek/deepseek-chat");
  assert.equal(c.thinking, "low");
  const d = await post(b, "/sessions", { model: "deepseek/deepseek-reasoner", default: false });
  assert.equal((await get(b, "/defaults")).model, "deepseek/deepseek-chat");
  assert.equal(d.model, "deepseek/deepseek-reasoner");
});

test("the triple is re-sent on session_start", async () => {
  const b = await startBench();
  const a = await post(b, "/sessions", { model: "deepseek/deepseek-chat", thinking: "high" });
  const sent = fakePi.commandsFor(a.id);
  assert.ok(sent.some((c) => c.type === "set_model" && c.modelId === "deepseek-chat"));
  assert.ok(sent.some((c) => c.type === "set_thinking_level" && c.level === "high"));
});
```

- [ ] **Step 2: Run → fails (no route).**
- [ ] **Step 3: Implement**
  - `providers.ts`: extend each entry with `wired: id === "deepseek"`; add the env/OAuth providers pi lists (`amazon-bedrock`, `azure-openai-responses`, `google-vertex`, `anthropic-oauth`, `github-copilot`, `openai-codex`) with `wired: false` and `label` per pi's `docs/providers.md`.
  - `rpc-child.ts`: `applyTriple`, `models`. In `bench.ts`, where a child answers `session_start` (the same place `setActiveTools` is triggered in the extension — the bench side is where `agent_start`/`get_state` are handled), call `applyTriple(rowTriple(id))` where `rowTriple` = session fields with defaults filled.
  - `bench.ts` route handlers: `setModel(id, body)`: if `body.default !== false` → `defaults.set(body)`; `sessions.update(id, body)`; `applyTriple`; `emit({type:"sessions"})`. Session create: `triple = {...defaults.get(), ...body}`; store it; `default:false` skips the defaults write.
  - `server.ts`: the three routes; bench token on all as siblings.
- [ ] **Step 4: Tests pass; typecheck.**
- [ ] **Step 5: Commit** `"Pick a model per session and re-apply it when pi starts"`

## Task 3: `/model` dialog, `Ctrl+T`, `Ctrl+E`, footer

**Files:**
- Create: `harness/src/renderer/components/ModelDialog.tsx`
- Modify: `harness/src/renderer/live.ts`, `Chat.tsx`, `keys.ts`, `rows.ts`
- Test: `harness/bench/test/renderer-rows.test.ts`, `harness/bench/test/renderer-boot.test.ts` (boot still clean)

**Interfaces:**
- Consumes: routes from Task 2.
- Produces: `live.models()` (cached `GET /models`), `live.setModel(session, patch)`; `modeParts(mode, model, thinking?, effort?)` → `{mode, model, thinking?, effort?}`.

- [ ] **Step 1: Test `modeParts`**

```ts
test("footer segments appear only when set", () => {
  assert.deepEqual(modeParts("build", "deepseek/deepseek-reasoner", "high", undefined),
    { mode: "build", model: "deepseek/deepseek-reasoner", thinking: "thinking high" });
  assert.deepEqual(modeParts("build", "x", undefined, "max"), { mode: "build", model: "x", effort: "effort max" });
});
```

- [ ] **Step 2: Run → fails on arity.**
- [ ] **Step 3: Implement**
  - `rows.ts`: `modeParts` as above; `modeLine` joins segments with ` · `.
  - `keys.ts`: `thinking: { keys: "^T", label: "thinking level", match: (e) => e.ctrlKey && !e.metaKey && key(e, "t") }`, `effort: { keys: "^E", … "e" }`. Note `⌘T` (switch workspace) stays: `meta()` matches ctrl **or** cmd today, so make `workspaces` require `e.metaKey` explicitly to free `^T`.
  - `ModelDialog.tsx`: rendered in the composer slot when `live.dialog() === "model"`. Three columns in one `pane font-mono` block at cell size: providers (unwired dimmed, `text-muted`, suffix `not configured`), models for the selected provider, and — when the model has `effort` — an effort row. Filter box on top; `↑↓` move, `Enter` picks (calls `live.setModel(thread.session, {model})`), `Esc` closes. Same row grammar as the permission prompt (`0b5fd022`).
  - `Chat.tsx`: slash menu gains `/model`; `^T` cycles `THINKING` filtered to what the model supports (`models()` capability), `^E` cycles `EFFORT`, both through `live.setModel` so the default moves too; footer uses the new parts; a segment absent renders nothing.
- [ ] **Step 4: Typecheck, `bench:test`, `npm run build`, boot test.**
- [ ] **Step 5: Commit** `"Choose a model with /model; Ctrl+T and Ctrl+E cycle thinking and effort"`

## Task 4: Ship slice 1

- [ ] Push `platform desktop-login`; pull in pod; `deploy/dev/ship.sh --no-gate` (harness only — fix the `.dockerignore` whitelist to admit `harness/skills` first, the current blocker); `deploy/pin.sh`; `kubectl apply` the four k3s yamls; `deploy/roll.sh`; recreate bench pods; relaunch the desktop. Verify: `/model` lists DeepSeek wired, pick persists across bench restart.

---

# Slice 2 — Shell sidecar and hands-free sessions

## File structure

- Create `deploy/shell-image/Dockerfile` — debian-slim + `kl` + `ttyd` + zsh/starship rc + gitignore-global; no sshd, no tool server.
- Modify `.github/workflows/image.yml` — build/push `kloudlite-shell:{sha}`; `deploy/pin.sh` rewrites its pin.
- Modify `crates/workspaces/src/k8s/mod.rs` — `SHELL_CONTAINER = "shell"`, `SHELL_PORT: u16 = 7790`, `shell_container(image, profile_mounts) -> Container`.
- Modify `crates/workspaces/src/k8s/workspace.rs` — `workspace_pod` appends `shell_container`; profile volume shared.
- Modify `crates/workspaces/src/k8s/bench.rs` — bench pod = `sessions` (renamed from `bench`) + `shell`; no `workspace` container; `{ws}` mounted in `sessions` only; token file `0400` via init container.
- Modify `crates/workspaces/src/k8s/netpol.rs` (or where `allow-bench-tools` lives) — admit `SHELL_PORT`.
- Modify `bins/agent/src/controller/workspace/bench.rs` — container name `sessions` for the idle/lock channel.
- Modify `crates/workspaces/src/crd/settings.rs` — `shell_image: Mark::Boot`.
- Delete `crates/ide/src/pty.rs` routes; remove `/stream/pty*` from `server.rs`.
- Harness: `bench/src/pty.ts` → splice to `{pod}:7790` speaking ttyd frames; `src/renderer/components/TerminalView.tsx` ttyd protocol; delete `desktop-terminal-reconnect.test.ts`, `desktop-terminal-tabs.test.ts` reconcile parts.
- Harness: `pi/kloudlite.ts` drop `ownTools`, bash registration, shell gate; `pi/workspace-tools.ts` no `bash`; identities gain §3.5 paragraph; `skills/workspaces.md` Packages section drops the bench target.
- Probes: `bins/slo/src/stages/bench.rs` + `workspace.rs` — `shell.up`, `shell.fenced`, `shell.no_tools`, `bench.no_hands`, `bench.pkg_needs_workspace`.

## Task 5: Shell image

**Files:** Create `deploy/shell-image/Dockerfile`, `deploy/shell-image/prelude.sh`; modify `.github/workflows/image.yml`, `deploy/pin.sh`.

- [ ] **Step 1:** Dockerfile: `FROM debian:bookworm-slim`; copy `kl` from the workspace image build stage (same stage `image.yml` uses); copy `deploy/workspace-image/{zshrc,starship.toml,gitignore-global}`; `useradd -u 1000 kl`; entrypoint `prelude.sh`.
- [ ] **Step 2:** `prelude.sh`: wait until `/nix/var/nix/profiles/kl/current/bin/ttyd` exists (profile symlink the agent publishes; poll 1 s, log once at 30 s); `exec ttyd -p 7790 -i 0.0.0.0 -W -t disableLeaveAlert=true -t fontFamily='IBM Plex Mono' -t fontSize=13 zsh -l` with `cd $HOME`. Comment: `-W` because ttyd is read-only without it.
- [ ] **Step 3:** `image.yml`: new matrix entry `shell` → `ghcr.io/kloudlite/kloudlite-shell:${sha}`; `deploy/pin.sh` learns the pin (`KLOUDLITE_SHELL_IMAGE` env on the agent DaemonSet in `deploy/k3s/agent-daemonset.yaml`, beside `KLOUDLITE_BENCH_IMAGE`).
- [ ] **Step 4:** `bins/agent/src/nix.rs::base_packages` appends `ttyd` and `bubblewrap` (slice 3 uses bwrap; adding both once avoids a second profile rebuild across the fleet).
- [ ] **Step 5: Commit** `"Build a shell image: kl, ttyd, the person's shell rc, nothing else"`

## Task 6: Shell container on both pod kinds

**Files:** `crates/workspaces/src/k8s/{mod.rs,workspace.rs,bench.rs}`, `crates/workspaces/src/k8s/tests/*.rs`, `crates/workspaces/src/crd/settings.rs`, `bins/agent/src/controller/workspace/bench.rs`, `crates/workspaces/src/model.rs` (`shell_container_resources()` 50m/64Mi, charged by `quota` like the bench's).

- [ ] **Step 1: Tests** in `k8s/tests/`:

```rust
#[test]
fn every_pod_carries_a_shell_that_sees_only_the_home() {
    let pod = workspace_pod(/* fixture */);
    let shell = container(&pod, SHELL_CONTAINER);
    let mounts: Vec<&str> = shell.volume_mounts.iter().flatten().map(|m| m.name.as_str()).collect();
    assert_eq!(mounts, ["home", "homecache", "profile"]);
    assert!(!mounts.contains(&"workspaces"));
    assert_eq!(shell.ports.as_ref().unwrap()[0].container_port, SHELL_PORT as i32);
}

#[test]
fn the_bench_pod_has_sessions_and_shell_and_no_workspace_container() {
    let pod = bench_pod(/* fixture */);
    let names: Vec<&str> = pod.spec.containers.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["sessions", "shell"]);
    let sessions = container(&pod, "sessions");
    assert!(mounts(sessions).contains(&"workspaces"));
    assert!(!mounts(sessions).contains(&"home"));
}
```

- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement** `shell_container(image: &str) -> Container`: image from `ClusterSettings.shell_image` (new `Mark::Boot` field, env bootstrap `KLOUDLITE_SHELL_IMAGE`), mounts `home` at `HOME_DIR`, `homecache` subPaths as the workspace container, `profile` read-only, port 7790, `resources: shell_container_resources()`, `security_context` as the workspace container (uid 1000, no privilege). `workspace_pod` pushes it. `bench.rs`: the pod is `[sessions_container(..), shell_container(..)]`; `sessions` keeps today's bench mounts (`workspaces` volume for `{ws}/.bench`, `BENCH_TOOL_PATH` secret) and **drops** `home`; the init container that writes the token file sets mode `0400` owner `root` (`fsGroup` unchanged; the harness reads it at start as root? — no: run `harness-bench` as uid 1000 and mount the secret with `defaultMode: 0o400` and `items` so only the file exists; the *model* has no tool to read it, which is the boundary; comment this). Controller `bench.rs` reads `containerStatuses[name=sessions]`. `quota.rs` charges the shell like the bench container.
- [ ] **Step 4: Gate.**
- [ ] **Step 5: Commit** `"Every workspace and bench pod carries a home-only shell sidecar"`

## Task 7: Fence 7790 and retire the PTY routes

**Files:** `crates/workspaces/src/k8s/` netpol builder (grep `allow-bench-tools`), `crates/ide/src/{server.rs,pty.rs,lib.rs}`, `crates/ide/tests/*` pty tests, `bins/slo/src/stages/workspace.rs` (`ws.terminal.persists` → deleted from catalogue + `deploy/slo.md` + web fixture).

- [ ] **Step 1: Test:** `allow_bench_tools_admits_ide_and_shell_ports` asserts ports `[IDE_PORT, SHELL_PORT]`; `no_pty_route` asserts `GET /stream/pty` is 404.
- [ ] **Step 2: Implement:** netpol ports; delete `pty.rs` and its three routes; remove `pty` from `lib.rs`; drop `ws.terminal.persists` and `bench.shell.workspace`'s reattach assertion (probe now dials ttyd, Task 9).
- [ ] **Step 3: Gate; commit** `"Fence the shell port and retire the tool server's PTY"`

## Task 8: Desktop terminal over ttyd; bench splice

**Files:** `harness/bench/src/pty.ts`, `harness/bench/src/server.ts` (`/pty?scope=`), `harness/src/renderer/components/TerminalView.tsx`, tests `desktop-terminal-*.test.ts`, `bench/test/pty.test.ts`.

**Interfaces:** ttyd WebSocket: client → `"0"+data` input, `"1"+JSON{columns,rows}` resize; first client frame is `JSON{AuthToken:"",columns,rows}`; server → `"0"+data` output, `"1"+title`, `"2"+prefs JSON`. Subprotocol `tty`.

- [ ] **Step 1: Test** `pty.test.ts`: a fake ttyd server; `spliceWorkspaceShell(ws, address, first)` forwards the initial JSON, input as `0…`, resize as `1…`, output back unchanged; on upstream close the client gets code 1000 and no retry.
- [ ] **Step 2: Implement:** `pty.ts` dials `ws://{podIp}:7790/ws` with subprotocol `tty`; `server.ts` resolves scope → pod address (bench scope = own pod IP from `KL_POD_IP` downward-API env, workspace scope = `/v1/workspaces/{id}/tools` address with port swapped to 7790); drop `session=` and `/pty/sessions`. `TerminalView.tsx`: on open send the auth JSON; map xterm `onData` → `0`, `onResize` → `1`; on message strip the opcode; `1` sets the tab title; on close print `[shell ended — open a new one]` and disable input. Remove the reconcile-every-5 s and reconnect ladder; delete their tests.
- [ ] **Step 3: Gate; commit** `"Terminals are ttyd sockets to the shell sidecar"`

## Task 9: Sessions have no hands

**Files:** `harness/pi/kloudlite.ts`, `harness/pi/workspace-tools.ts`, `harness/pi/catalog.ts`, `harness/skills/workspaces.md`, `harness/bench/src/rpc-child.ts`, tests `bench-tools.test.ts`.

- [ ] **Step 1: Tests:** in every mode `getAllTools()` has no `bash`, `read`, `write`, `edit`, `grep`, `find`, `ls`, `process` bound to the local machine — for the bench session there are **none** of those names at all (the ide names exist only in workspace/agent mode and dial a pod); `kl_pkg_add` without `workspace` → error "name the workspace"; the identity text contains the §3.5 paragraph and, for the bench, "You have no working directory. Name a workspace."; `tool_search("shell")` on the bench answers "no tool for that here".
- [ ] **Step 2: Implement:** delete `ownTools`, the `127.0.0.1:7788` binding, the `harness:shell-gate` and its allow-list; `ALWAYS_ON` for the bench = `ask plan skill tool_search memory question architecture report` (+ `kl_*` deferred); workspace/agent `ALWAYS_ON` = the ide set (dialling the pod) + the same; `kl_pkg_*`/`kl_env_*` require `workspace`. Identity paragraphs verbatim from spec §3.5. `skills/workspaces.md` Packages: remove "on the bench".
- [ ] **Step 3: Gate; commit** `"No session has hands where it runs"`

## Task 10: Probes and ship slice 2

- [ ] Add `shell.up`, `shell.fenced`, `shell.no_tools` (asserts **401** from 7788 inside the shell, per spec §2.5), `bench.no_hands`, `bench.pkg_needs_workspace` to `catalogue.rs` + `deploy/slo.md` + fixture; implement in `stages/bench.rs`/`workspace.rs` (`shell.*` dial ttyd through the tunnel helper the bench probes use; `bench.no_hands` sends "cat /etc/hostname" and asserts no tool call in the transcript).
- [ ] Rust gate on the laptop; harness gate; push; ship `--no-gate`; pin; apply k3s yamls (agent DS gains `KLOUDLITE_SHELL_IMAGE`); roll; **recreate every bench pod and workspace pod by hand** (owner's `!` lines if the classifier blocks deletes); relaunch desktop; hand-start `hourly-manual-HHMM`; update `docs/capacity-model.md` with the shell container.

---

# Slice 3 — Subagent trees

## File structure

- Modify `crates/workspaces/src/crd/workspace.rs` — `TreeSpec`, `TreeStatus`, `spec.trees`, `status.trees`; `crd/settings.rs` `trees_per_workspace: u32 = 8` (*proposed*).
- Create `crates/workspaces/src/api/workspaces/trees.rs` — `POST/DELETE /v1/workspaces/{id}/trees[/{name}]`.
- Create `bins/agent/src/controller/workspace/trees.rs` — reconcile step: snapshot/delete nested subvolumes; status.
- Modify `bins/agent/src/peer/sweeps.rs` — orphan `.agents/*` sweep; `cleanup_parent` deletes trees first.
- Modify `deploy/workspace-image/gitignore-global` — `.agents/`.
- Modify `crates/ide/src/{paths.rs,api.rs,tools/*,fs/*,procs.rs,server.rs}` — `tree` param, `TreeCtx` map, main excludes `.agents/`, tree-relative output, no home prefix; `exec` under `bwrap`; ports.
- Create `crates/ide/src/sandbox.rs` — the bwrap argv builder, one function, unit-tested as argv.
- Harness: `bench/src/bench.ts` agent path (`kl_agent_run` → tree), `sessions.ts` `tree?`, ide binding pins `tree`; `pi/kloudlite.ts` `kl_agent_close`; renderer sidebar nesting, Files tab `?tree=`, "Diff against main".
- Probes: `ws.tree.cut`, `ws.tree.isolated`, `ws.tree.no_travel`, `ws.tree.ports`, `ws.tree.closed`, `agent.tree.run`.

## Task 11: CRD fields and `/v1` routes

**Files:** `crd/workspace.rs`, `crd/settings.rs`, `api/workspaces/trees.rs`, `api/workspaces/mod.rs` (router), `crates/workspaces/tests/api_trees.rs`, `deploy/k3s/crds.yaml` (regenerate), `deploy/k3s/agent-admission.yaml` (the policy must allow the agent to write `status.trees` — status subresource, already allowed — and **not** `spec.trees`).

**Interfaces:**
```rust
pub struct TreeSpec { pub name: String, pub created: Time }
pub struct TreeStatus { pub name: String, pub path: String, pub ready: bool, pub reason: Option<String> }
pub fn tree_name_ok(s: &str) -> bool   // ^[a-z0-9-]{1,32}$
```
Routes: `POST /v1/workspaces/{id}/trees {name}` → 202 `{name, path}`; 409 `"tree {name} exists"`, `"the workspace is not running; a tree is cut from a live one"`, `"trees: {n} of {limit} in use"`; 422 bad name. `DELETE …/trees/{name}` → 202; 404 unknown.

- [ ] **Step 1: Tests** (`api_trees.rs`, the `api_bench.rs` harness shape): create on Running → 202 and spec has it; second same name → 409; 9th → 409 with `8`; on Stopped → 409; DELETE removes spec entry; bad name → 422.
- [ ] **Step 2: Implement** per spec §4.2; `may_act_on` + spec write through the same JSON-merge path as `attachedEnvironment`.
- [ ] **Step 3: Gate; commit** `"A workspace can be asked for a tree"`

## Task 12: Node agent cuts and deletes trees

**Files:** `bins/agent/src/controller/workspace/trees.rs`, `mod.rs` (call after pod Running), `bins/agent/src/peer/sweeps.rs`, `bins/agent/src/controller/workspace/cleanup.rs` (`cleanup_parent`), `bins/agent/tests/reconcile/trees.rs`.

- [ ] **Step 1: Tests** (the reconcile test harness with a fake `Btrfs`): spec tree + no status → `snapshot(src={ws}, dst={ws}/.agents/x)` called once, status `ready:true, path`; status without spec → `delete`; snapshot error → `ready:false, reason` and retried next pass; `cleanup_parent` with two trees deletes both before the worktree; sweep deletes `.agents/y` when the spec has no `y`.
- [ ] **Step 2: Implement:** `btrfs subvolume snapshot {ws} {ws}/.agents/{name}` (create `.agents` dir `0755` uid 1000 first); `btrfs subvolume delete`; status write through the existing status patch path; sweep: `for d in {ws}/.agents/*` if `is_subvolume(d) && !spec.trees.contains(name)` → delete (re-read spec before delete, keep on error — same rule as `retire_pass`).
- [ ] **Step 3: Gate; commit** `"The node agent cuts a tree as a nested snapshot and collects it"`

## Task 13: `tree` in the tool server, tree-relative paths, bubblewrap

**Files:** `crates/ide/src/{paths.rs,api.rs,server.rs,procs.rs,sandbox.rs,fs/*.rs,tools/*.rs}`, tests in `crates/ide/tests/`.

**Interfaces:**
```rust
pub struct TreeCtx { pub name: String, pub root: PathBuf, pub graft: Graft, pub watcher: Watcher, pub port_block: Option<(u16,u16)> }
pub fn tree_of(state: &State, q: Option<&str>) -> Result<Arc<TreeCtx>, ToolError>  // None|"main" → main; else {ws}/.agents/{name} must exist
pub fn confine(tree: &TreeCtx, given: &str) -> Result<PathBuf, ToolError>          // relative only; main refuses .agents/*
pub fn relative(tree: &TreeCtx, p: &Path) -> String                                  // strips root for every output
pub fn bwrap_argv(tree: &TreeCtx, profile: &Path, cmd: &[String]) -> Vec<String>    // sandbox.rs
```

- [ ] **Step 1: Tests:** `confine` rejects `/home/kl/x` with 400 "paths are relative to your working directory"; `main` rejects `.agents/x/f` with 403 naming `.agents/x/f`; tree `x` accepts `src/a.rs` → `{ws}/.agents/x/src/a.rs`; `relative()` strips; `bwrap_argv` equals the spec §4.7 argv for a tree; `exec` env contains `KL_TREE=x`, `PORT=20100`, `KL_PORT_RANGE=20100-20199` and **not** `KL_WORKSPACE`/`HOME`(of the pod); process listing from tree `x` excludes main's ids; a detached process whose ring shows `EADDRINUSE` is marked `failed: port N in use by main: {cmd}`.
- [ ] **Step 2: Implement:** `State.trees: RwLock<HashMap<String, Arc<TreeCtx>>>`, lazy on first `tree=` use, dropped when the dir is gone (checked on each use). Every `/tools/{name}` body and `/fs/*` query accepts `tree`. `exec`: `Command::new("bwrap").args(bwrap_argv(..))` with `--setenv HOME {tree}/.home` (mkdir once), cwd the tree; if `bwrap` is missing from the profile, log once and run unwrapped (the fleet verifies, spec §4.7). Port block: `20000 + 100*i` for the i-th tree by creation order in the map (main none). EADDRINUSE: scan the first 4 KiB of the ring after exit for `EADDRINUSE` or `address already in use`, resolve the port from the command's `--port`/`-p`/`PORT=` hint, look up `/proc/net/tcp` inode → pid → which tree's process table holds it, else "another process".
- [ ] **Step 3: Gate; commit** `"Serve every tree of a workspace; wrap each exec in bubblewrap"`

## Task 14: Agents run in trees (harness)

**Files:** `harness/bench/src/bench.ts`, `sessions.ts` (`tree?`), `rpc-child.ts` (ide binding carries `tree`), `pi/kloudlite.ts` (`kl_agent_run`, `kl_agent_close`, remove the clone path and `clones` map), `src/renderer/components/{Sidebar,WorkView}.tsx`, tests `agents.test.ts`.

- [ ] **Step 1: Tests:** `kl_agent_run` → `POST /v1/workspaces/{ws}/trees` then a session with `kind:"ephemeral", tree:name, workspace: ws`; ide calls from that session carry `tree:name` and a call whose args say `tree:"main"` is rewritten to `name` (bench test `ws.tree_pinned`); `kl_agent_close` is a proposal, then `DELETE …/trees/{name}` and the session archived; no `/v1/workspaces/{id}/clone` call anywhere in the agent path (assert the fake platform never saw one).
- [ ] **Step 2: Implement** per spec §4.3; sidebar nests agent sessions under the workspace as `{name} · {status}`; Files tab passes `?tree=`; CHANGES from `/fs/changes?tree=`; "Diff against main" = `exec git diff main...HEAD` read-only rendered as a diff card. Delete the clone-based agents code and its tests.
- [ ] **Step 3: Gate; commit** `"Agents work in trees of their workspace, not in cloned workspaces"`

## Task 15: Probes and ship slice 3

- [ ] Add the six probe ids (spec §4.8) to the catalogue, `deploy/slo.md`, fixture; implement in `stages/workspace.rs` (`ws.tree.*`) and `stages/bench.rs` (`agent.tree.run`, the bench-driven one: dispatch → tree exists → report → tree stays → close → gone).
- [ ] `.agents/` into `deploy/workspace-image/gitignore-global`.
- [ ] Rust gate; harness gate; push; ship `--no-gate`; pin; apply CRDs + admission + agent DS; roll **agents first** (finalizer race memory); recreate pods; relaunch desktop; hand-start hourly. Record bwrap behaviour under the runtime class in the spec §4.7 as observed.

---

## Self-review

- Spec coverage: §1 → Tasks 1–3; §2 → 5–8; §3 → 9 (+6 token mode, +8 no PTY); §3.5 → 9 identity, 13 relative paths/env; §4.2 → 11–12; §4.3/§4.5 → 14; §4.4/§4.6/§4.7 → 13; §4.8 probes → 15; §2.5/§3.4 probes → 10; §8 retirements → 7, 8, 9, 14.
- Names used consistently: `Triple`, `Defaults`, `applyTriple`, `SHELL_PORT`, `SHELL_CONTAINER`, `TreeSpec/TreeStatus`, `TreeCtx`, `tree_of`, `confine`, `relative`, `bwrap_argv`.
- Open detail for Task 2: whether the locked pi exposes effort as a `set_model` option; the implementer checks `rpc.md` in the bench image's pi and reports which branch was taken.
