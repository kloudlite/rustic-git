# Consent From the Person's Own Words Implementation Plan

> **For agentic workers:** steps use checkbox (`- [ ]`) syntax. Work in `/Volumes/kdisk/rustic-git-wt/consent`
> (branch `consent`, off master). Prefix every shell command with `cd /Volumes/kdisk/rustic-git-wt/consent/harness &&`.

**Goal:** a gated tool call the person literally asked for this turn runs without a permission card; every
other gated call shows a card carrying the model's reason (or the false claim). `container_build` asks.

**Architecture:** every gated tool gets a required `because` argument (`{asked}` | `{reason}`), injected in
`registryFor` and stripped before the tool runs. `LocalBackend` records the text the person typed through a
CLIENT view (only `serve.ts` opens those) per session key, cleared at the end of the turn it fed. The gate
checks `because.asked` against those words with the pure `consented()`; failing that it sends `reason` /
`claimed` on the `PermissionRequest` and the TUI prints them on the card.

**Tech Stack:** TypeScript, Bun (`bun test`), pi agent.

**Spec:** `docs/superpowers/specs/2026-10-09-workspace-egress-fence-design.md` §4 and §5.

## Global Constraints

- Commits: imperative sentence case, no tool attribution, no trailers.
- Comments explain WHY; deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>`.
- Never `git commit -a`; add files by name.

## Rulings (controller, 2026-10-09)

- "Client view" is NOT "opened with `permission`": `delegate.ts` opens internal views with `permission` too.
  `SessionOpts.client?: boolean` is set only by `serve.ts`.
- Turn scoping: words typed before a turn's `agent_start` are dropped at its `agent_end`; a followUp/steer
  typed mid-turn survives into the next turn. `// ponytail:` a steer pi consumes in the same turn lingers one
  extra turn.
- Card wording: the subtitle already names action and target, so the consent line is `Why: {reason}` /
  `Says you asked: “{quote}”, which is not in your messages this turn` / `No reason given`.
- Image target = last path segment of the tag, tag/digest stripped (`team/hello:1` → `hello`).
- Program target skips leading `VAR=value` words; `cmd` may be a string or argv array (exec), `command` (bash).

---

### Task 1: consent.ts (pure check + turn words)

**Files:** Create `harness/packages/backend/src/consent.ts`, `harness/packages/backend/src/consent.test.ts`.

**Produces:** `BECAUSE_SCHEMA`, `type Because`, `target(name, args, self?)`, `consented(name, args, because, typed, self?)`, `class TurnWords { add(text); start(); end(); get(): string[] }`.

- [ ] Write `consent.ts`:

```ts
//! Consent from the person's own words (spec 2026-10-09-workspace-egress-fence §5). Owner: "if user
//! himself ask to do something then there is no need to ask for permission again". A teller does what
//! you ask at the counter and phones you, with the reason, when someone else asks in your name. Only
//! text typed through a client view counts (`TurnWords`, fed by LocalBackend); a goal another session
//! wrote, a web page, a file or a tool result never does.

/** The argument every gated tool requires; the gate reads it and the tool never sees it. */
export const BECAUSE_SCHEMA = {
  type: "object",
  description:
    "Why this call happens; the person sees it on the permission card. `asked`: the person's exact words from their message THIS turn, only when they literally asked for this action, quoted verbatim and naming its target. Otherwise `reason`: one sentence on why the task needs it.",
  properties: { asked: { type: "string" }, reason: { type: "string" } },
} as const;

export type Because = { asked?: unknown; reason?: unknown };

const norm = (s: string) => s.toLowerCase().replace(/\s+/g, " ").trim();
const image = (tag: unknown) => (typeof tag === "string" ? tag.split("/").pop()!.split(/[:@]/)[0] || undefined : undefined);

/** The program a command runs: basename of its first word, leading `VAR=value` words skipped. */
function program(cmd: unknown): string | undefined {
  const words = Array.isArray(cmd) ? cmd.map(String) : typeof cmd === "string" ? cmd.trim().split(/\s+/) : [];
  const first = words.find((w) => !/^[A-Za-z_][A-Za-z0-9_]*=/.test(w));
  return first?.split("/").pop() || undefined;
}

/** What the quote must name for this call; undefined means no rule, so the call asks. `self` is the
 * workspace a workspace session's tool defaults to when its `workspace` argument is absent. */
export function target(name: string, args: any, self?: string): string | undefined {
  switch (name) {
    case "workspace_stop":
    case "workspace_delete":
      return args?.workspace ?? self;
    case "worktree_drop":
      return args?.name;
    case "env_delete":
    case "env_stop":
    case "env_restore_in_place":
      return args?.env;
    case "service_remove":
      return args?.service;
    case "volume_delete":
      return args?.volume;
    case "snapshot_delete":
      return args?.snapshot;
    case "container_build":
      return image(args?.tags?.[0]);
    case "container_push":
      return image(args?.src);
    // ponytail: the quote binds the program, not its arguments (`cargo` covers any `cargo …`); the
    // network fence is the wall that matters for exec
    case "exec":
      return program(args?.cmd);
    case "bash":
      return program(args?.command);
    case "web_fetch":
      try {
        return new URL(String(args?.url)).hostname;
      } catch {
        return undefined;
      }
  }
  return undefined;
}

/** Skip the card only when `because.asked` (8+ chars) is in one thing the person typed this turn AND
 * names the call's target. Case and whitespace are normalised on both sides. */
export function consented(name: string, args: any, because: Because | undefined, typed: string[], self?: string): boolean {
  const q = typeof because?.asked === "string" ? norm(because.asked) : "";
  if (q.length < 8 || !typed.some((t) => norm(t).includes(q))) return false;
  const t = target(name, args, self);
  return typeof t === "string" && t.length > 0 && q.includes(norm(t));
}

/** What the person typed, for the turn it feeds. `start` at agent_start marks what that turn read;
 * `end` at agent_end drops it, so a followUp typed mid-turn still counts for the next turn.
 * ponytail: a steer pi consumes in the same turn lingers one extra turn; key entries by turn if that bites. */
export class TurnWords {
  #words: string[] = [];
  #mark = 0;
  add(text: string) {
    this.#words.push(text);
  }
  start() {
    this.#mark = this.#words.length;
  }
  end() {
    this.#words.splice(0, this.#mark);
    this.#mark = 0;
  }
  get(): string[] {
    return this.#words;
  }
}
```

- [ ] Write `consent.test.ts` (bun:test), at least these cases, each its own `test`:
  - `consented("workspace_delete", {workspace:"foo"}, {asked:"please delete workspace foo"}, ["Please  DELETE workspace foo now"])` → true (case/whitespace).
  - quote without target: `{asked:"delete it now please"}`, typed `["delete it now please"]`, args `{workspace:"foo"}` → false.
  - quote not typed: `{asked:"delete workspace foo"}`, typed `["stop workspace bar"]` → false.
  - short quote `{asked:"foo"}` typed `["foo"]` with `{workspace:"foo"}` → false.
  - `reason` only → false.
  - self default: `consented("workspace_stop", {}, {asked:"stop this workspace demo"}, ["stop this workspace demo"], "demo")` → true.
  - exec: `{cmd:"cargo test -p x"}` with asked `"run cargo test please"` typed same → true; `{cmd:"curl https://x.io"}` same quote → false; argv `{cmd:["/usr/bin/cargo","build"]}` with asked `"run cargo build"` → true; `{cmd:"FOO=1 cargo test"}` target is `cargo`.
  - bash uses `command`: `target("bash", {command:"ls -la"})` → `"ls"`.
  - web_fetch host: `{url:"https://docs.rs/tokio"}` asked `"read docs.rs for tokio"` → true; `{url:"https://evil.com/docs.rs"}` same quote → false; bad URL → target undefined.
  - container_build `{tags:["team/hello:1"]}` → target `"hello"`; container_push `{src:"hello@sha256:ab"}` → `"hello"`.
  - unknown tool name → `target` undefined, `consented` false.
  - `TurnWords`: add "a"; start; add "b" (mid-turn); end → get() is `["b"]`; start; end → `[]`.
- [ ] Run `bun test packages/backend/src/consent.test.ts` → all pass.
- [ ] Commit `consent.ts consent.test.ts`: "Check consent against the person's own words".

### Task 2: schema, strip, gate, typed words, container_build

**Files:** Modify `harness/packages/backend/src/local.ts`, `harness/packages/backend/src/index.ts`,
`harness/packages/backend/src/serve.ts`, `harness/packages/backend/src/local.test.ts`.

**Consumes:** Task 1 exports.

- [ ] `index.ts`: `PermissionRequest` gains
  `/** The model's one-sentence reason (because.reason). */ reason?: string;` and
  `/** A quote the model said the person typed but that failed the check (because.asked). */ claimed?: string;`.
  `SessionOpts` gains `/** Set only by serve.ts: this view is a person at a client, so what is typed through it counts as their words (consent.ts). */ client?: boolean;`.
- [ ] `serve.ts` `session.open`: pass `client: true` alongside `permission`.
- [ ] `local.ts`: add `"container_build"` to `ALWAYS_ASK` (it runs the person's Dockerfile on their builder with an open network). Add:

```ts
/** Every tool `mustAsk` can be true for: each carries a required `because` (consent.ts). */
export const GATED = new Set([...ALWAYS_ASK, ...ASK_UNLESS_FENCED]);

/** A gated tool with `because` in its schema, removed before the tool runs: no tool ever sees it. */
export function asking(t: ToolDef): ToolDef {
  if (!GATED.has(t.name)) return t;
  const s: any = t.inputSchema ?? {};
  return {
    ...t,
    inputSchema: { ...s, type: "object", properties: { ...s.properties, because: BECAUSE_SCHEMA }, required: [...(s.required ?? []).filter((n: string) => n !== "because"), "because"] },
    run: (input: any) => {
      const { because: _, ...rest } = input ?? {};
      return t.run(rest);
    },
  };
}
```

  `registryFor`: every branch passes its tool list through `.map(asking)` before `r.add(...)` (e.g. `r.add(...[webFetch, webSearch, ...].map(asking))`).
- [ ] `installGate(agent, permission, fence?, words?: () => { typed: string[]; self?: string })`; `ask` becomes:

```ts
  const ask = async (ctx: any, signal: AbortSignal) => {
    const name = ctx.toolCall.name;
    if (!mustAsk(name, fence?.())) return undefined;
    const { because, ...args } = ctx.args ?? {};
    const w = words?.();
    if (w && consented(name, args, because, w.typed, w.self)) return undefined;
    const said = (v: unknown) => (typeof v === "string" && v.trim() ? v.trim() : undefined);
    const decision = await permission!(
      { name, args, diff: toolDiff(name, args) ?? undefined, reason: said(because?.reason), claimed: said(because?.asked) },
      signal,
    );
    return decision.block ? decision : undefined;
  };
```

- [ ] `LocalBackend`: field `#typed = new Map<string, TurnWords>()` with helper `#words(key)` that gets-or-creates. In `#attach`, when `opts.client`, the returned view's `prompt`/`steer`/`followUp` call `this.#words(key).add(text)` then delegate to `v`'s method (keep `dispose` wrapping as is; `#attach`'s early return must not skip a client view: return early only when `!opts.permission && opts.tools.length === 0 && !opts.client`). In `session()`, right after `const handle = baseHandle(...)`: `handle.subscribe((e) => { if (e.type === "agent_start") this.#words(key).start(); else if (e.type === "agent_end") this.#words(key).end(); });`. Pass to `installGate` the fourth argument `() => ({ typed: this.#words(key).get(), self: k.kind === "main" ? undefined : k.ws })`.
- [ ] `local.test.ts`: house test expects `ALWAYS_ASK.size` 11 and lists `container_build`. Add tests:
  - `asking()` on a fake gated tool (`name:"workspace_delete"`, schema `{type:"object",properties:{workspace:{type:"string"}},required:["workspace"]}`, run records its input): schema has `properties.because` and `required` `["workspace","because"]`; `run({workspace:"x", because:{reason:"r"}})` reaches the tool as `{workspace:"x"}`; a non-gated tool (`read`) comes back identical (same object).
  - `installGate` with a fake agent `{ agent: { beforeToolCall: undefined } }`: typed `["please delete workspace foo"]`, call `agent.agent.beforeToolCall({toolCall:{name:"workspace_delete"}, args:{workspace:"foo", because:{asked:"delete workspace foo"}}})` → permission never called, returns undefined. Same with typed `[]` → permission called once with `claimed: "delete workspace foo"` and args without `because`. With `because:{reason:"cleanup after test"}` → request carries `reason`.
- [ ] `bun test packages/backend` → all pass; `bunx tsc --noEmit -p packages/backend` (or the package's tsconfig) clean.
- [ ] Commit the four files: "Skip the card for calls the person asked for and show the reason otherwise".

### Task 3: TUI card and the dead GATED set

**Files:** Modify `harness/apps/tui/src/app.tsx`.

- [ ] Delete the unused `GATED` set (~line 871; defined, never read). Keep `EDITS`.
- [ ] In `gate()`, destructure `reason, claimed` from the request. Compute

```ts
    const why = reason
      ? `Why: ${reason}`
      : claimed
        ? `Says you asked: “${claimed}”, which is not in your messages this turn`
        : "No reason given";
```

  and make the card's `body` start with `why`: `body: [why, <existing body expression>].filter(Boolean).join("\n\n")`. Read how the card renders `body` next to `diff` first; if a card with `diff` hides `body`, put `why` in the subtitle instead (`${subtitle} · ${why}`) for diff cards only.
- [ ] Typecheck the TUI (`bunx tsc --noEmit -p apps/tui` or its package script) and `bun test apps/tui` → pass.
- [ ] Commit `app.tsx`: "Show why on the permission card".

## Review Focus

- A gated call from a codemode script (nested `_beforeToolCall` path) carries `because` too and is stripped by `asking().run`.
- Claude sessions: the tool server reads the registry schema, so `because` must appear there and be stripped in `run`.
- A workspace session opened by `workspace_ask` (internal view, no `client`) never records words.
- `exec` with `cmd` as argv array.
- A client view's `prompt` still returns the agent's promise (awaited by serve.ts).
