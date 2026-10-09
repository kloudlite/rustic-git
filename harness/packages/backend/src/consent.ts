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
export function target(name: string, args: any, self?: string): string | string[] | undefined {
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
    case "packages_remove": {
      const names = (args?.packages ?? []).map((p: unknown) => String(p).split("@")[0]);
      return names.length ? names : undefined;
    }
    case "service_update":
      return args?.service?.name;
    case "intercept":
      return args?.service;
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
  const all = (Array.isArray(t) ? t : [t]).filter((x): x is string => typeof x === "string" && x.length > 0);
  return all.length > 0 && all.length === (Array.isArray(t) ? t.length : 1) && all.every((x) => q.includes(norm(x)));
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
