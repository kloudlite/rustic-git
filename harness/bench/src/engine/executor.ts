import { existsSync } from "node:fs";
import { join } from "node:path";
import { Type } from "@sinclair/typebox";
import { choice, noul, type Ask, type Answer, type ChoiceAnswer, type NoulAnswer } from "./jev.ts";
import { chooserQuestions, decide, ladder } from "./chooser.ts";
import { TOOLS, procIds, TALK, RESPONSES, runTool, plainValue, ACT, DECIDE, type User, type Tool } from "./tools.ts";
import { tryRemote } from "./remote.ts";
import { child, root, MAX_DEPTH, JEV_FRAME_CAP, type Envelope } from "./task.ts";
import { pickBlocks, formatBlocks } from "./pick.ts";
import { compress } from "./headroom.ts";
import { freeFromLiterals } from "./router.ts";
import type { Summarise } from "./sections.ts";

export type ActDeps = { ask: Ask; cwd: string; readOnly?: boolean; think?: (question: string) => Promise<string>; log?: (text: string) => void; approve?: (tool: string, args: Record<string, string>, destructive: boolean) => Promise<boolean>; user?: User; generate?: (prompt: string, envelope: Envelope) => Promise<string>; tools?: Tool[] };
export type ActInput = { instruction: string; detail?: string; params?: Record<string, string>; command?: string };
export type ActOpts = { signal?: AbortSignal };

const indent = (depth: number) => "  ".repeat(depth);

const READ_GATE = {
  changes: noul("Would carrying out the instruction change the project: add, write, edit, delete or move code or files, install packages, change git, or start or stop a server? Running the tests, a build or a type check only reports and is not a change."),
  costly: noul("Does the instruction execute something that costs time and resources to get information (run the tests, a script, a build, a type check)? Only looking at what is already there (listing files, reading or searching a file, git status or diff, the output of a running process) does not."),
};
// One call, whatever way its args were spelt: seen live, a declined {"command":"other","shell":"php tests/run.php"} came back as {"shell":"php tests/run.php"} and was asked again.
const callKey = (tool: string, args: Record<string, string>) => `${tool} ${JSON.stringify(Object.entries(args).filter(([, v]) => v !== "other").sort())}`;
const NOT_LOOKUPS = ["bash", "write", "edit", "kill_shell", "tell_user", "ask_user", "think"];
const READ_ONLY = "refused: reading mode. Only look at what is already there (list files, read or search a file, git status or diff, the output of a process already running); changes are never made while planning, and the user did not allow anything costlier: go on without it: put what you wanted done into what you submit.";
const NO_LOOKUPS = ["read"]; // tools whose params writer answers in one turn, from its prompt alone
const SELF_LOOKUPS = ["read", "bash"]; // tools whose params session may call the tool itself to look things up
const MAX_READ_DEPTH = MAX_DEPTH - 1; // envelope depth up to which a read params session may itself read
const MAX_GENERATE_ACTS = 6; // lookups per params session; past this the session is told to submit
// The step judge's question never changes (Jev warms per question block); the step's own stop outcomes travel in the state.
const JUDGE = {
  decision: choice("What should happen after this step?", {
    next: "The step did what it was meant to do, and the output shows what the state lists as expected, when it lists anything; go on",
    stop: "The output shows one of the stop_outcomes listed in the state, which ends the task early",
    review: "The step went wrong (error, nothing usable, wrong tool); the user must decide",
  }),
};
// A failed write, move or delete throws and carries no label, so a labelled one did what it says. Seen live: Jev unsure about "wrote README.md" cost a replan each time.
// With no stop outcomes in the state the stop label has nothing to match and only takes votes from review: it is left out. Two shapes, so both blocks stay warm.
const JUDGE_NO_STOP = { decision: choice("What should happen after this step?", Object.fromEntries(Object.entries((JUDGE.decision as { criteria: Record<string, string> }).criteria).filter(([k]) => k !== "stop"))) };
const SELF_EVIDENT = ["tell_user", "kill_shell", "glob", "bash_output", "write", "edit"];
const MAX_PLANS = 6; // planner calls per task
const MAX_LOOKS = 5; // lookups per task, however many calls they come in // lookups per research session; past this the session is told to submit
const PICK_OVER = 2000; // chars of tool output above which only the relevant parts go back to the LLM
// An envelope frame is cut at 2,000 chars from its start, which kept the oldest step and dropped the newest: the params writer and Jev's param picks
// lost exactly the output they needed. The newest steps are kept instead, each cut to fit.
const STEP_CHARS = 700;
const FRAME_BUDGET = 1700; // under the frame cap, leaving room for JSON escaping
export function recentSteps(results: string[], budget = FRAME_BUDGET, each = STEP_CHARS): string[] {
  const out: string[] = [];
  let n = 0;
  for (const r of [...results].reverse()) {
    const t = r.slice(0, each);
    if (n + t.length > budget) break;
    out.unshift(t);
    n += t.length;
  }
  return out;
}

const EARLIER_STEPS = 5; // how many earlier steps the tool pick sees
const EARLIER_CHARS = 8000; // of each, head and tail kept. Probed live on 10 steps: 300 and whole both picked 10 of 10; whole was surer on one (write 0.60 to 0.78), less sure on none
const PICK_SKIP = ["read"]; // read already picks

// Any large tool output (grep hits, command output, a process log, a diff) is cut down to the parts relevant to the step, with line numbers,
// before it reaches the LLM: its tokens are resent on every later turn. Nothing relevant found, or Jev down: the output goes back as it was.
// headroom compresses first (structural: json/search/diff/log shape), jev picks second (semantic: what this step needs) on whatever's left.
async function relevant(out: string, tool: string, instruction: string, deps: ActDeps, ind: string): Promise<string> {
  const c = compress(out, instruction);
  out = c.text;
  if (out.length <= PICK_OVER || PICK_SKIP.includes(tool) || /^(blocked|failed|denied|error)/.test(out)) return out;
  const { blocks } = await pickBlocks(deps.ask, out, instruction, { trace: (l) => deps.log?.(`${ind}  ${l}`) });
  const picked = formatBlocks(`${tool} output`, blocks);
  if (blocks.length === 0 || picked.length >= out.length) return out;
  return `${picked}\n\n(only the parts of the ${out.split("\n").length}-line output relevant to this step; ask again more specifically for other parts)`;
}

// Every reader of a step's output (planner, writer, thinker, the step judge, the drafter) is told what it is looking at: a bare "p1" or an empty diff says nothing alone.
// Refusals and errors stay bare: code matches on their prefix.
export function labelled(tool: Tool, args: Record<string, string>, out: string): string {
  if (/^(denied|blocked|error|started in background)/.test(out)) return out;
  const shown = Object.entries(args).map(([k, v]) => (v.length > 120 || v.includes("\n") ? `${k}=(${v.length} chars)` : `${k}=${v}`)).join(" ");
  return `[${tool.name}${shown ? ` ${shown}` : ""}] ${tool.shows ?? ""}\n${out || "(no output)"}`;
}

export function makeAct(deps: ActDeps, context: () => unknown, envelope?: () => Envelope | undefined) {
  const reads = new Set<string>(); // lookups already made by this actor; see runTool
  const refused = new Map<string, string>(); // exact calls the user declined or that failed, and why
  const act = async ({ instruction, detail, params = {}, command }: ActInput, earlier: string[] = [], opts?: ActOpts): Promise<string> => {
    // A caller with no envelope (main's own session) still gets one, so params can be built and the instruction reaches the tool.
    const parent = envelope?.() ?? root(instruction, context(), []);
    const ind = indent(parent.depth + 1);
    try {
      // The tool is picked from the step's instruction plus what the earlier steps did ("Stop it": 0.70 alone, 0.99 with them).
      // The task itself stays out: with it in view Jev judges the whole task ("find, delete, then show" -> none) instead of the one step; params still get the chain.
      const seen = earlier.length > 0 ? { earlierSteps: earlier.slice(-EARLIER_STEPS).map((r) => headTail(r, EARLIER_CHARS)) } : {};
      // Reading mode (the planner and the thinker researching): Jev is the gate, not the prompt, and the gate rides on the tool pick: the same call that picks the tool
      // also rules on the lookup. One that changes something is rejected outright (planning never mutates); one that executes something costing time and resources
      // to get information is escalated to the user.
      let escalated = false; // reading mode wants more than a lookup: only the user can allow it
      // A params session must not call the tool it is building params for, except read: finding read's path means reading other files, and with read dropped
      // Jev answered none or run for every "read the file X" (37 acts, 80s, in one session). Depth bounds the nesting.
      // run likewise: a command's values can depend on live output (the ids a curl returns); reading mode puts every such run to the user.
      const without = SELF_LOOKUPS.includes(parent.generateTool ?? "") && parent.depth < MAX_READ_DEPTH ? undefined : parent.generateTool;
      // A step that carries its own literal command is picked alone. Seen live: after a write step, the earlier steps pulled it to write (0.82) and the test output was written to a file named "-".
      const alone = !!command;
      const answers = await deps.ask({ instruction, context: alone ? {} : seen }, { ...chooserQuestions(deps.tools ?? TOOLS), ...(deps.readOnly ? READ_GATE : {}) });
      if (deps.readOnly) {
        const changes = (answers.changes as NoulAnswer).noul, costly = (answers.costly as NoulAnswer).noul;
        if (changes >= DECIDE) { deps.log?.(`${ind}jev: a change, rejected in reading mode (${changes.toFixed(2)})`); return READ_ONLY; }
        // Not a change, but it consumes time and resources: the user decides whether the plan is worth it.
        if (costly >= DECIDE) { deps.log?.(`${ind}jev: costly lookup (${costly.toFixed(2)})${deps.approve ? ": asking the user" : ""}`); if (!deps.approve) return READ_ONLY; escalated = true; }
      }
      const d = decide(answers, deps.tools ?? TOOLS);
      if (deps.readOnly && d.kind !== "none" && NOT_LOOKUPS.includes(d.tool.name)) {
        // Only run can serve a plan (its output is information), so only run goes to the user; writing, stopping, talking and thinking are rejected.
        if (!deps.approve || d.tool.name !== "bash") { deps.log?.(`${ind}jev: ${d.tool.name} refused in reading mode`); return READ_ONLY; }
        escalated = true;
      }
      if (d.kind === "none") {
        deps.log?.(`${ind}jev: no tool matched`);
        return `No tool matches. Available: ${(deps.tools ?? TOOLS).map((t) => `${t.name} (${t.description})`).join("; ")}. Rephrase as one concrete step; if it is an action no tool covers, make it a bash step with the exact command in its command field; if it only decides something, it is not a step.`;
      }
      const conf = d.confidence !== undefined ? ` (${d.confidence.toFixed(2)})` : "";
      deps.log?.(`${ind}jev: ${d.tool.name}${conf}`);
      if (without === d.tool.name) {
        deps.log?.(`${ind}skipped ${d.tool.name}: params are being built`);
        return `You are building the params for \`${d.tool.name}\`; do not call it. Call submit_params.`;
      }
      const names = d.tool.params.map((p) => p.name);
      // A backticked literal in the step is the tool's one free param (read's exact search text), as on main's path.
      const given = { ...freeFromLiterals(d.tool, instruction), ...Object.fromEntries(Object.entries(params).filter(([k]) => names.includes(k))) };
      // A task takes no input: a confirm decision with no approve hook wired is denied, not silently run.
      const approve = d.kind === "confirm" || escalated ? (deps.approve ? (args: Record<string, string>) => deps.approve!(d.tool.name, args, d.destructive) : async () => false) : undefined;
      let built = given;
      const ctx = {
        user: deps.user,
        envelope: parent && { ...child(parent, instruction, { instruction, outcomes: d.tool.outcomes }, [...d.tool.outcomes, "other"]), ...(command ? { command } : {}) },
        ask: deps.ask,
        generate: deps.generate,
        // The same tool with the same args that the user already declined, or that already failed, is not put to them again. Seen live: a denied
        // `php tests/run.php` was offered again nine seconds later, and a failing command was rerun verbatim.
        approve: approve && (async (args: Record<string, string>) => {
          // A shell command that only looks (git status, ls, cat) is not put to the user: Jev votes on the command as written, and both
          // votes must be as sure as any act-without-asking vote. A package script is never waved through: its name does not show what it runs.
          // ponytail: one wrong vote runs an unseen command; the bar is 1 - ACT on both questions, raise it or drop this block if that ever happens.
          if (d.tool.name === "bash" && !d.destructive && args.shell && (!args.command || args.command === "other")) {
            const v = (await deps.ask({ instruction: `run the shell command: ${args.shell}` }, READ_GATE).catch(() => ({}))) as Record<string, NoulAnswer | undefined>;
            const changes = v.changes?.noul ?? 1, costly = v.costly?.noul ?? 1;
            if (changes <= 1 - ACT && costly <= 1 - ACT) { deps.log?.(`${ind}jev: only looks, not asked (changes ${changes.toFixed(2)}, costly ${costly.toFixed(2)})`); return true; }
          }
          const ok = await approve(args);
          if (!ok) refused.set(callKey(d.tool.name, args), "declined by the user");
          return ok;
        }), soft: !escalated && d.kind === "confirm" && d.soft,
        trace: deps.log, detail,
        onArgs: (a: Record<string, string>) => {
          built = a;
          const why = refused.get(callKey(d.tool.name, a));
          if (why) throw new Error(`blocked: this exact ${d.tool.name} call was already ${why}; do something different or ask the user`);
        },
        seen: reads,
        think: deps.think && ((q: string) => deps.think!(`${q}\n\nContext:\n${JSON.stringify(parent).slice(0, 4000)}`)),
        signal: opts?.signal,
      };
      const ran = await runTool(deps.cwd, d.tool, given, ctx);
      // A failed call is only a repeat while nothing has changed: a test command that failed is rightly run again after an edit.
      const failedRun = /^(error|failed|no answer)|\[exit code [1-9]/.test(ran.slice(0, 300)) || /\[exit code [1-9]\d*\]$/.test(ran);
      if (failedRun) refused.set(callKey(d.tool.name, built), "run and failed with nothing changed since");
      else if (NOT_LOOKUPS.includes(d.tool.name)) for (const [k, why] of refused) if (why.startsWith("run and failed")) refused.delete(k);
      if (deps.readOnly && ran.startsWith("denied")) return READ_ONLY;
      return labelled(d.tool, built, await relevant(ran, d.tool.name, detail ? `${instruction}\n${detail.slice(0, 600)}` : instruction, deps, ind));
    } catch (e) {
      const msg = (e as Error).message;
      return /^(blocked|failed|denied)/.test(msg) ? msg : `error: ${msg}`;
    }
  };
  return async (input: ActInput, earlier: string[] = [], opts?: ActOpts) => {
    const ind = indent(envelope?.()?.depth ?? 0);
    deps.log?.(`${ind}act "${input.instruction}"`);
    const out = await act(input, earlier, opts);
    if (/^(blocked|failed|denied)/.test(out)) deps.log?.(`${ind}  ${out.slice(0, 200)}`);
    return out;
  };
}

// The LLM is handed in, as Jev's ask is: the library names no provider and no SDK. One call is one fresh session with no history: a system
// prompt, the tools it may call, then prompt(). A tool result with terminate: true ends the session with no closing reply.
export type LlmTool = { name: string; label: string; description: string; parameters: unknown;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  execute: (id: string, input: any) => Promise<{ content: { type: "text"; text: string }[]; details: object; terminate?: boolean }> };
export type LlmMessage = { role: string; stopReason?: string; errorMessage?: string; content: { type: string; text?: string }[] };
export type LlmSession = { prompt: (text: string) => Promise<unknown>; messages: LlmMessage[]; model?: { provider: string; id: string } };
export type Llm = (cwd: string, system: string, tools: LlmTool[]) => Promise<LlmSession>;
const defineTool = (t: LlmTool) => t;

const GENERATE_SYSTEM = `Hand over the requested params by calling submit_params once. Write no text: no explanation, no reasoning, no quoted code; every extra word costs tokens. You may call act first to look up values you cannot know (current time: run the command \`date\`; file contents: ask for the part you need, e.g. "find the / route in src/app.js"). Do NOT perform the task yourself; act is only for looking up values.
Never guess what the project defines: a route, a method, a field name, a port, a script, a path. When a value talks to the project's own code (a curl against its API, an edit to its file), read that code first with act (the project files are listed) and use exactly what it defines. A change to an existing file is written from the file as it is now: when its current text is not in front of you, read it with act first, and make old_string match the file's exact text, copied verbatim, with enough surrounding lines that it occurs exactly once. When a lookup fails or comes back empty, look another way (another path, a search for the name) before settling; never submit an error text as a value. When the code does not settle it, submit your best value rather than ask. When a value depends on live output (the ids an API returns), get it with act and pass the exact command yourself: params {"shell": "<the exact command>"}; the user is asked first, and when they refuse, submit your best value.`;

// Fills a tool's missing params in a one-shot session.
// Its own act calls run against the given child envelope, so they are depth-bounded like any other nesting.
// The research tool of the planner and the thinker: every project tool, in reading mode. Planning reads the current state; a lookup that needs more (running something, changing something) is put to the user through the approve hook, and refused when they say no or none is wired.
// What a task's planner and thinker looked up, kept for the task: each of them is a fresh one-shot session, so a replan or a think
// otherwise reads the same files again. Only labelled outputs are kept (a refusal or an error is not knowledge).
export type Looks = { n: number; seen: string[] };
export const remember = (looks: Looks, out: string) => { if (out.startsWith("[") && !looks.seen.includes(out)) looks.seen.push(out); return out; };
// A file a step wrote is no longer what was seen: what was looked up in it is dropped, so it is read again when needed.
export const forget = (looks: Looks, out: string) => { const path = /^\[write path=(\S+)/.exec(out)?.[1]; if (path) looks.seen = looks.seen.filter((x) => !x.includes(path)); };
export function seenText(looks: Looks): string {
  const text = looks.seen.map((x) => headTail(x, LLM_CHARS)).join("\n\n");
  return text ? `\n\nAlready looked up for this task (current; do not look these up again):\n${text}` : "";
}

export function makeLook(deps: { llm: Llm; cwd: string; ask: Ask; log?: (text: string) => void; approve?: ActDeps["approve"] }, looks: Looks = { n: 0, seen: [] }) {
  const act = makeAct({ ...deps, readOnly: true, generate: makeGenerate({ ...deps }) }, () => ({}));
  return defineTool({
    name: "look", label: "look", description: "Look things up in the project. Pass every lookup you need in ONE call: each later call resends everything seen so far. At most " + MAX_LOOKS + " lookups for the whole task, later planning included; past that a lookup returns nothing. Each lookup is one thing in plain language, naming the exact file when known (\"read the file the task names\", \"read the handlers in src/main.<ext>\"); look up only what this task needs. Long results come back as their most relevant parts, with the line ranges of the rest. Reading only: nothing can be changed.",
    parameters: Type.Object({ instructions: Type.Array(Type.String()) }),
    execute: async (_id, input) => {
      const outs = await Promise.all((input as { instructions: string[] }).instructions.map(async (instruction) =>
        `${instruction}:\n${++looks.n > MAX_LOOKS ? "lookup limit reached: submit now with what you know" : remember(looks, await act({ instruction }))}`));
      return { content: [{ type: "text", text: outs.join("\n\n---\n\n") }], details: {} };
    },
  });
}

export function makeGenerate(deps: { llm: Llm; cwd: string; ask: Ask; log?: (text: string) => void; approve?: (tool: string, args: Record<string, string>, destructive: boolean) => Promise<boolean>; user?: User;
  // what the task's planner already looked up: the writer is shown it, and adds what it reads, so no file is read twice for one task
  looks?: Looks }) {
  return async (prompt: string, envelope: Envelope): Promise<string> => {
    const ind = "  ".repeat(envelope.depth);
    deps.log?.(`${ind}generate [${(envelope.generate ?? []).join(", ")}] -> llm`);
    // The writer only needs information: its act is in reading mode, like the planner's look.
    const act = makeAct({ ...deps, readOnly: true }, () => ({}), () => envelope);
    let acts = 0;
    const actTool = defineTool({
      name: "act",
      label: "act",
      description: "Carry out one concrete step described in plain language.",
      parameters: Type.Object({
        instruction: Type.String(),
        params: Type.Optional(Type.Record(Type.String(), Type.String())),
      }),
      execute: async (_id, input) => ({ content: [{ type: "text", text: ++acts > MAX_GENERATE_ACTS ? "lookup limit reached: call submit_params now with your best values" : deps.looks ? remember(deps.looks, await act(input)) : await act(input) }], details: {} }),
    });
    // The params come back as a tool call, not as prose to parse; terminate ends the session with no closing reply.
    let submitted: Record<string, string> | undefined;
    const submitTool = defineTool({
      name: "submit_params",
      label: "submit_params",
      description: "Hand over the requested params. Call this once, as your last step.",
      // One fixed schema: tool definitions open the provider's cached prefix, so a schema built per call would break the cache for every params call. The prompt names the params.
      parameters: Type.Object({ params: Type.Record(Type.String(), Type.String()) }),
      execute: async (_id, input) => { submitted = (input as { params: Record<string, string> }).params; return { content: [{ type: "text", text: "ok" }], details: {}, terminate: true }; },
    });
    // A read's params are a path and a query: the file list in the prompt settles them. With act, the writer spent 6 turns of lookups to choose a path.
    const session = await deps.llm(deps.cwd, GENERATE_SYSTEM, NO_LOOKUPS.includes(envelope.generateTool ?? "") ? [submitTool] : [actTool, submitTool]);
    await session.prompt(prompt + (deps.looks ? seenText(deps.looks) : ""));
    if (submitted) return JSON.stringify(submitted);
    const last = session.messages.filter((m) => m.role === "assistant").at(-1);
    if (last?.stopReason === "error") throw new Error(`${session.model?.provider}/${session.model?.id}: ${last.errorMessage}`);
    const text = last ? last.content.filter((c) => c.type === "text").map((c) => (c as { text: string }).text).join("") : "";
    if (text.trim()) deps.log?.(`${ind}  llm: ${text.slice(0, 300).replace(/\n/g, " ")}`);
    return text;
  };
}

// The LLM plans a task once, up front, as steps that each carry the outcomes that end the task early.
// Between steps there is no LLM: Jev picks each step's tool and classifies its output; the LLM only fills free params.
export type Step = { id: string; needs: string[]; instruction: string; detail?: string; expect?: string; stopOn?: Record<string, string>; command?: string;
  // Set on a top-level node that is a subplan: the node runs its own graph of steps instead of one tool call.
  sub?: { doneWhen: DoneWhen[]; steps?: (Partial<Step> & { instruction: string })[] } };
export type DoneWhen = { fact: string; files?: string[] };
// A plan is a graph of subplans, a subplan is a graph of steps, a step is one tool call: three levels, never more.
// steps is optional on a subplan: the planner fills it only when it already knows them; left out, the subplan is planned
// when its turn comes. A flat `steps` plan (a small task, a subplan's own planning, a test fixture) runs as it always did.
type LooseSteps = (Partial<Step> & { instruction: string })[];
export type LooseSubplan = { id?: string; needs?: string[]; goal: string; doneWhen?: DoneWhen[]; steps?: LooseSteps };
export type Plan = { steps: Step[]; doneWhen: DoneWhen[]; complete?: boolean; answer?: string };
export type LoosePlan = { steps?: LooseSteps; subplans?: LooseSubplan[]; doneWhen: DoneWhen[]; complete?: boolean; answer?: string };
export type Planner = (task: string) => Promise<LoosePlan>;
export const MAX_SUBPLAN_PLANS = 3; // planner calls per subplan, its first planning call counted

// An old-style plan (a test fixture, or a planner reply with no ids) is a flat sequence: each step needs the one before it,
// which keeps existing plans meaningful with no fixture churn. A plan where only SOME steps carry an id is the planner's
// error (silently filling the rest loses the ordering it meant), caught here before normalising and sent back like any
// other invalid graph.
export function normalisePlan(steps: (Partial<Step> & { instruction: string })[]): Step[] {
  const hasIds = steps.some((s) => s.id);
  if (hasIds && steps.some((s) => !s.id)) throw new Error("some steps carry an id and some do not: give every step an id, or none");
  return steps.map((s, i) => ({ ...s, id: hasIds ? (s.id ?? `s${i}`) : `s${i}`, needs: hasIds ? (s.needs ?? []) : (i > 0 ? [`s${i - 1}`] : []) }));
}

// Unknown ids in `needs`, and a cycle, are the planner's error to fix: caught here, not left for the scheduler to spin on.
// A patch may name a done step's id in needs (already satisfied, so it can never cycle): those ids are given in `done`.
export function validatePlan(steps: Step[], done: string[] = []): string | undefined {
  const ids = new Set([...steps.map((s) => s.id), ...done]);
  for (const s of steps) for (const n of s.needs) if (!ids.has(n)) return `step "${s.id}" needs unknown step "${n}"`;
  const state = new Map<string, "visiting" | "done">();
  const visit = (id: string): string | undefined => {
    if (state.get(id) === "done") return undefined;
    if (state.get(id) === "visiting") return `cycle in the plan graph at step "${id}"`;
    const step = steps.find((s) => s.id === id);
    if (!step) return undefined; // a done id: already run, so it can never be part of a cycle
    state.set(id, "visiting");
    for (const n of step.needs) { const err = visit(n); if (err) return err; }
    state.set(id, "done");
    return undefined;
  };
  for (const s of steps) { const err = visit(s.id); if (err) return err; }
  return undefined;
}
export const MAX_CRITERIA = 4;

const PLAN_SYSTEM = `Plan the task by calling submit_plan once. Write no text.
A small task is a plan of steps: give steps and leave subplans out. A big task, one with several pieces of work that each take their own steps, is a plan of subplans: give subplans and leave the top-level steps out. A subplan has an id ("A", "B", ...), needs (ids of subplans that must finish first; subplans with no needs edge between them run at the same time), a goal (one plain sentence saying what this piece of work achieves, naming the files it is about) and its own done_when facts. Give a subplan its steps only when you already know them from what you looked up; leave steps out when they depend on what an earlier subplan finds or does, and you will be called for that subplan alone when its turn comes, with the results of the subplans it needs. There are never more than these three levels: when the situation opens with "Subplan of a larger task", answer with steps only.
Each step has an id, a short instruction and an optional detail. id is a short string unique in the plan ("s1", "s2", ...). needs lists the ids of steps that must finish before this one can start; leave it empty when the step can start at once. Steps with no needs edge between them may run at the same time, so give a needs edge whenever a step uses another step's result or touches the same file as it: two steps that write the same file need an edge between them, or they may collide. Give the plan as much parallelism as is actually safe: independent steps (reading different files, running unrelated commands) need nothing. Shell commands run this way too: two bash steps with no needs edge between them start at the same time as separate processes, so give a needs edge between any two commands that would otherwise collide over the same thing (the same folder's install, the same port, a test run while a write it depends on is still in flight).
Plan only as far as you can know. Do not guess steps that depend on results you have not seen; a short graph is fine, you will be called again with the results of the steps that ran. When steps have already run, never plan one of them again: the situation lists them as done with their result, and a new step may name a done step's id in needs (it is already satisfied).
The instruction is ONE concrete action in one plain sentence of at most 20 words that names what it acts on and nothing else: no code, no field names, no how ("find the login handler in src/main.<ext>", "delete the route found in the previous step from src/main.<ext>", "run the tests"). A separate system picks the tool for each step and fills its params; a step sees the output of the steps it needs. Use the fewest steps; never add a step to verify or re-read unless the task asks to see something, or the situation lists a fact as "Not yet": then one step that shows that fact is allowed.
Planning is three things: take the requirement, check the current state, decide the direction. Check the state with the look tool: list the files and read the ones the task touches (and existing output or results already on disk, when they matter), until you can name exact files and have made every decision (where code goes, the schema, the approach). look is for the information the plan needs; a refused lookup means plan without it. Then submit a plan whose steps only act: no step that merely looks at what you already saw. Name exact files in every step.
A step changes ONE file, and its instruction names only that file. Moving code between files is one step per file ("add the parsing functions to src/parse.<ext>", then "remove the parsing functions from src/main.<ext>"); the other file and the code taken from it go in the detail.
Every step must be something a tool below does. When no other tool fits an action, or a tool has already failed at it, the shell is always available: plan it as a bash step, its exact command in the step's command field (\`mv src/old src/new\`). A plan that changes code ends with a run step of the project's own test command when a lookup showed the exact command (a script entry, a build target, a documented runner); before planning a change, look for that command (the project's manifest, its readme). When no lookup showed a test command, a plan that changes code ends with whatever else checks the change: a build, type-check or start command a lookup showed, and failing those a read of each changed file; and done_when says that command passes; changed code is never called done untested. A tool that reports its own success (kill_shell names what it stopped, write and edit name what they changed) needs no step after it to check that it happened; a check step is for what the code does, not for whether a tool call took effect. Check narrowly first: the test or command closest to what changed, then the project's whole check when one is known. When a test fails, the fix goes in the code under test: never loosen, skip or delete a test to make it pass, unless the user said the test itself is wrong. A failure the task did not ask about and the plan's own changes did not cause (a test that already failed) is told to the user at the end, not fixed, and done_when does not wait on it. Commands use the project's own tools when it has them (.venv/bin/pytest, node_modules/.bin, a package.json script), not a global one. Every bash step carries its exact command in the command field (\`.venv/bin/pytest -q\`), never just "run the tests"; when the command is not known yet, an earlier step lists the files to find it. One step is one tool call on one file: moving or editing two files is two steps, and a whole directory is moved or renamed by one bash step (\`mv old new\`) on the directory itself, followed by a search for the old name and an edit of each file that still uses it. Throwing away uncommitted changes is one bash step (\`git restore .\`, plus \`git clean -fd\` when new files were added), never an edit step that types the old content back. New behaviour (a route, a function, a flag) gets a test step in the project's existing test file when the project has tests. An edit step says the change itself and the file ("make total in src/cart.js skip cancelled items"), never just "fix the bug", and opens with a verb of change (make, change, add, replace): "load the settings at startup in src/config.py" would be taken for reading the file. Do what the task asks and nothing more. A task that asks to look, analyze, review or find issues changes nothing, whatever the lookups show: it has no steps: once the lookups have shown enough, call submit_plan with complete: true and answer holding what was found, in full, as the reply the user reads; the user decides what to fix. A plan never carries a step that only tells the user something: the last step's output and the closing line are shown to the user already. Only a report that something is broken (a failing test, an error) with nothing else asked means find the cause and fix it. done_when covers what this plan's steps do, not work finished before it. A step asks the user only what the project cannot show: what a read, a search or the test command answers is looked up, not asked, and a question that got no answer is not asked again. Never plan a step that only decides, designs, chooses, considers or verifies by thinking ("decide the schema", "choose the storage"): make the decision yourself now and write it into the detail of the step that acts on it; the think tool is only for a choice that depends on what an earlier step finds (instruction "create src/store.<ext> with the item store", detail "an in-memory store keyed by id, items {id, title, done}, with create, read, update and delete").
command, on a step, is the exact shell command when this step runs one; copied as written, not described, with no backticks around it.
When a lookup showed something a later task would have to look up again (the exact start, test or build command and its folder, where a kind of code lives), pass it in notes, one short line each; never a note about this task\'s own work or state. When the situation lists facts as "Not yet", check them with look before planning. If they are in fact true and nothing is left to do, call submit_plan with complete: true and no steps. Otherwise plan only the steps that make them true.
detail is where everything else goes, and only the writer of that step's params sees it: what you found with look that the step needs (the exact lines of code to move or change, quoted with their file and line numbers), and every decision you made (names, schema, shape of the code, what to keep and what to drop). An existing file is never changed unread: a step that changes one comes after a lookup or a read step of that file. The writer has not read what you read: a step that creates or changes code from existing code carries that code in its detail, so nothing is read twice. Steps that need no detail (run the tests, show the diff) carry none.
To search every file for an exact text, put that text in backticks in the instruction; otherwise say what to find and the right files are found for you.
stopOn maps an outcome that must end the task early to the reason told to the user, e.g. {"nothing found": "there is no / route"}. Steps whose outcome cannot end the task carry no stopOn. A failing test, build or command is never a stopOn outcome: a failure is replanned, and fixed or reported.
expect, on a step, is one line saying what that step's output shows when the step worked ("the file is written", "the tests pass").
Every explicit requirement of the task is covered by a step and by a done_when fact; none is quietly left out. done_when lists 1 to 4 facts that are true once the whole task is finished, each at most 20 words and naming the file or command it is about ("src/parse.<ext> exists and holds the parsing functions", "src/main.<ext> uses the parsing functions from src/parse.<ext>"). Each item has a fact and, when the fact is about one or more files, a files list naming exactly those files ({"fact": "src/parse.<ext> exists and holds the parsing functions", "files": ["src/parse.<ext>"]}); a fact about a command (a test run) carries no files. A separate system checks each fact against the step outputs, using files to find the evidence. A fact says what a file contains or what a command prints, never how the code behaves in a case no step runs: behaviour is shown by a test, and the fact is that the test command passes. A process started in the background only shows it is alive: a fact that a service runs or serves is shown by a later run step whose command asks it something and prints the reply, and the fact names that command.
Tools the steps can use: ${TOOLS.filter((t) => t.name !== "tell_user").map((t) => `${t.name} (${t.brief ?? t.description})`).join("; ")}.`;

// What one task found out about the project (its start and test commands, where things live) is kept for the next task, which otherwise looks it all up again.
// Seen live: "start the server" and "stop the server", back to back, each began by listing the files and reading package.json; every such lookup turn is one more planner call.
// ponytail: a plain capped list, oldest lines dropped first, never checked against the project; the planner is told a note may be stale. A note that a failure disproves is not yet removed.
const NOTES = ".jevharn/project.md", MAX_NOTES = 30;
export const readNotes = async (cwd: string) => {
  const r = await tryRemote(cwd, "read", { path: NOTES });
  return typeof r === "string" ? [] : (r as { content: string }).content.split("\n").filter(Boolean);
};
export async function addNotes(cwd: string, notes: string[]) {
  const fresh = notes.map((n) => n.replace(/\s+/g, " ").trim().slice(0, 200)).filter(Boolean);
  if (fresh.length === 0) return;
  const all = [...new Set([...(await readNotes(cwd)), ...fresh])].slice(-MAX_NOTES);
  await tryRemote(cwd, "write", { path: NOTES, content: `${all.join("\n")}\n` }); // notes are a saving, never a failure: tryRemote turns a failed write into a string result, not a throw
}

export function makePlanner(llm: Llm, cwd: string, look?: () => ReturnType<typeof defineTool>): Planner {
  return async (task) => {
    let plan: LoosePlan | undefined;
    const submit = defineTool({
      name: "submit_plan", label: "submit_plan", description: "Hand over the plan. Call this once.",
      parameters: Type.Object({ steps: Type.Optional(Type.Array(Type.Object({ id: Type.Optional(Type.String()), needs: Type.Optional(Type.Array(Type.String())), instruction: Type.String(), detail: Type.Optional(Type.String()), expect: Type.Optional(Type.String()), stopOn: Type.Optional(Type.Record(Type.String(), Type.String())), command: Type.Optional(Type.String()) }))), subplans: Type.Optional(Type.Array(Type.Object({ id: Type.String(), needs: Type.Optional(Type.Array(Type.String())), goal: Type.String(), done_when: Type.Optional(Type.Array(Type.Object({ fact: Type.String(), files: Type.Optional(Type.Array(Type.String())) }))), steps: Type.Optional(Type.Array(Type.Object({ id: Type.Optional(Type.String()), needs: Type.Optional(Type.Array(Type.String())), instruction: Type.String(), detail: Type.Optional(Type.String()), expect: Type.Optional(Type.String()), stopOn: Type.Optional(Type.Record(Type.String(), Type.String())), command: Type.Optional(Type.String()) }))) }))), done_when: Type.Optional(Type.Array(Type.Object({ fact: Type.String(), files: Type.Optional(Type.Array(Type.String())) }))), complete: Type.Optional(Type.Boolean()), answer: Type.Optional(Type.String()), notes: Type.Optional(Type.Array(Type.String())) }),
      execute: async (_id, input) => { const i = input as { steps?: LooseSteps; subplans?: (Omit<LooseSubplan, "doneWhen"> & { done_when?: DoneWhen[] })[]; done_when?: DoneWhen[]; complete?: boolean; answer?: string; notes?: string[] };
        await addNotes(cwd, i.notes ?? []);
        const plain = (steps?: LooseSteps) => steps?.map((st) => ({ ...st, instruction: plainValue(st.instruction, "instruction") ?? "" }));
        plan = { steps: plain(i.steps), subplans: i.subplans?.map(({ done_when, ...sp }) => ({ ...sp, goal: plainValue(sp.goal, "goal") ?? "", doneWhen: (done_when ?? []).slice(0, MAX_CRITERIA), steps: plain(sp.steps) })), doneWhen: (i.done_when ?? []).slice(0, MAX_CRITERIA), complete: i.complete, answer: plainValue(i.answer, "answer") }; return { content: [{ type: "text", text: "ok" }], details: {}, terminate: true }; },
    });
    const session = await llm(cwd, PLAN_SYSTEM, look ? [look(), submit] : [submit]);
    // The file list skips ignored folders, so the planner never saw .venv and wrote a bare "pytest" that was not installed.
    // ponytail: three well-known spots; add more as stacks turn up.
    const own = [".venv/bin", "venv/bin", "node_modules/.bin"].filter((d) => existsSync(join(cwd, d)));
    // The planner starts each task with nothing carried over, so what runs in the background is told to it. Seen live: "stop server" spent three lookups and an lsof to find a process this session had started as p1.
    const bg = await procIds(cwd);
    const running = bg.length > 0 ? `\n\nBackground processes started here (bash_output reads one, kill_shell stops one): ${bg.join("; ")}` : "";
    const known = await readNotes(cwd);
    const notes = known.length > 0 ? `\n\nKnown about this project from earlier tasks (may be stale; look up only what is not here):\n${known.join("\n")}` : "";
    await session.prompt((own.length > 0 ? `${task}\n\nThe project's own tools are in: ${own.join(", ")} (run them by that path, for example ${own[0]}/<tool>)` : task) + running + notes);
    // A long task history makes the model answer in text instead of calling the tool: one nudge in the same session before giving up.
    if (!plan) await session.prompt("You did not call submit_plan. Call submit_plan now with the steps.");
    const last = session.messages.filter((m) => m.role === "assistant").at(-1);
    if (last?.stopReason === "error") throw new Error(`${session.model?.provider}/${session.model?.id}: ${last.errorMessage}`);
    if (!plan) throw new Error(`the planner submitted no plan; it said: ${JSON.stringify(last?.content ?? "").slice(0, 200)}`);
    // Ids/needs (unknown-id and cycle checks included) are normalised and validated once, uniformly, by runPlan's own plan() wrapper.
    return plan;
  };
}

const THINK_SYSTEM = `A step of a planned task needs one decision that no file, search or command can give. Decide it from the question and the context; use the look tool when the answer depends on a file you have not seen, and when a lookup fails, look another way before deciding. Call submit_message once with the decision in at most 400 characters: the choice made, concretely, ready to act on. Write no other text.`;
export const ANSWER_SYSTEM = `The user asked something about this project, and a lookup was already run for it; its output follows the question, possibly cut short. When what you need is cut or missing, use the look tool, then answer. Answer what the user meant, from that output: concrete, with file paths and line numbers, as a short list when several things are asked for. If the output does not hold the answer, use the look tool, then answer. When a lookup fails or finds nothing, look another way (another path, a search for the name) before giving up. Say plainly what could not be found; never guess. Call submit_message once with the answer. Write no other text.`;
const DRAFT_SYSTEM = `A planned task hit something the automatic decision maker could not settle. Draft the message to the user by calling submit_message once. Write no other text.
The message says in one or two short sentences what happened, then asks what to do. Do not propose a new plan and do not try to fix anything.
Say only what "Done so far" shows was done. The task text is what was asked, not what happened: never claim work (written, extended, fixed, started) that no step's output shows. If the steps only looked at things, say so.
When the problem lists facts that could not be confirmed, say which, and give no other cause.`;

// The LLM's only job when Jev is stuck: word the message to the user.
export type Drafter = (situation: string) => Promise<string>;

export function makeDrafter(llm: Llm, cwd: string, system = DRAFT_SYSTEM, look?: () => ReturnType<typeof defineTool>): Drafter {
  return async (situation) => {
    let message: string | undefined;
    const submit = defineTool({
      name: "submit_message", label: "submit_message", description: "Hand over the message to the user. Call this once.",
      parameters: Type.Object({ message: Type.String() }),
      execute: async (_id, input) => { message = plainValue((input as { message: unknown }).message, "message"); return { content: [{ type: "text", text: "ok" }], details: {}, terminate: true }; },
    });
    const session = await llm(cwd, system, look ? [look(), submit] : [submit]);
    await session.prompt(situation);
    const last = session.messages.filter((m) => m.role === "assistant").at(-1);
    if (last?.stopReason === "error") throw new Error(`${session.model?.provider}/${session.model?.id}: ${last.errorMessage}`);
    // Seen live: the model wrote the answer as plain text and never called submit_message. The text is the message; no second call.
    if (!message && Array.isArray(last?.content)) message = last.content.flatMap((c) => c.type === "text" ? [c.text] : []).join("\n").trim();
    if (!message) throw new Error("the drafter submitted no message");
    return message;
  };
}

const SECTION_SYSTEM = `A topic of the conversation just finished. Summarise it by calling submit_section once. Write no other text.
label: at most 6 words naming the topic ("move parsing to its own file").
summary: at most 600 characters: what was asked, what changed on disk, what is left open.`;

// The real section summariser: a one-shot LLM call, same shape as makeDrafter, over the closed section's full messages.
export function makeSummarise(llm: Llm, cwd: string): Summarise {
  return async (messages) => {
    let out: { label: string; summary: string } | undefined;
    const submit = defineTool({
      name: "submit_section", label: "submit_section", description: "Hand over the label and summary. Call this once.",
      parameters: Type.Object({ label: Type.String(), summary: Type.String() }),
      execute: async (_id, input) => { { const i = input as Record<string, unknown>; out = { label: plainValue(i.label, "label") ?? "", summary: plainValue(i.summary, "summary") ?? "" }; } return { content: [{ type: "text", text: "ok" }], details: {}, terminate: true }; },
    });
    const session = await llm(cwd, SECTION_SYSTEM, [submit]);
    await session.prompt(messages);
    const last = session.messages.filter((m) => m.role === "assistant").at(-1);
    if (last?.stopReason === "error") throw new Error(`${session.model?.provider}/${session.model?.id}: ${last.errorMessage}`);
    if (!out) throw new Error("the summariser submitted no section");
    return { label: out.label.split(/\s+/).slice(0, 6).join(" "), summary: out.summary.slice(0, 600) };
  };
}

const MAX_ASKS = 3;         // drafted questions per task; past this the raw problem is shown
export { DECIDE };

export type PlanDeps = { plan: Planner; draft: Drafter; act: (input: ActInput, results: string[], opts?: ActOpts) => Promise<string>; ask: Ask; askUser?: (question: string) => Promise<string>; log?: (t: string) => void;
  // Drains what the user typed for this run while it was busy. Read between steps: a running step is never interrupted.
  steer?: () => { lines: string[]; stop: boolean };
  // Is this path there now? Lets a fact with no evidence be read (free) before the planner is asked about it.
  exists?: (path: string) => boolean;
  // what the planner looked up for this task: the evidence for an answer given with no steps run
  evidence?: () => string[] };

// Jev judges an output whole: a failure in the middle of a long test run decides the verdict as much as the tail does.
// Jev's state limit is 32,000 tokens (over it: 400 max_tokens_exceeded). Probed live, 60,000 chars of test output are judged right in
// under a second. One call's outputs share JEV_CHARS, which with the task text stays under 60,000 chars, about 2 chars a token at the
// worst; only an output over its share is cut, head and tail kept.
export const JEV_CHARS = 50_000;
// Jev is free, the LLM is not: one result or lookup sent to the LLM is cut to this, head and tail kept.
export const LLM_CHARS = 6000;
// The task text is the earlier context followed by the instruction itself, so a cut keeps the END: a cut from the start handed the judges
// the old messages and dropped the very instruction they were judging.
const taskForJev = (task: string) => task.slice(-8000);
export const headTail = (out: string, max = JEV_CHARS) => (out.length > max ? `${out.slice(0, max >> 2)}\n… ${out.length - max} chars cut here; a read of the file or a narrower command shows them …\n${out.slice(-(max - (max >> 2)))}` : out);
export const ANSWERED = { borne: noul("Do the lookups bear out what the answer says, and does the answer give what the instruction asked for?") };
export const COMPLETE = { complete: noul("Do the results show the task was fully carried out?") };

// Runs a plan and returns the task's final answer ("done: ..." | "blocked: ...").
// Jev decides after every step. What Jev cannot settle pops to the user, in words drafted by the LLM.
// The LLM plans at the start and drafts messages; the planner runs again only on the user's own instruction.
// `done` holds steps that already ran (main's own tool call): nothing is planned until Jev says they do not complete the task.
// Facts are deduped by their text, not object identity: a replan naming the same fact again must not carry it twice.
const dedupeDoneWhen = (ds: DoneWhen[]): DoneWhen[] => [...new Map(ds.map((d) => [d.fact, d])).values()];

export async function runPlan(deps: PlanDeps, task: string, done: string[] = [], maxPlans = MAX_PLANS): Promise<string> {
  const log = deps.log ?? (() => {});
  const results: string[] = [...done];
  const verdicts: string[] = done.map((d) => `${d.split("\n")[0]}: ran`);
  let asks = 0;
  // Every replan is an LLM turn: past the cap the planner answers with nothing, which puts the task to the user.
  let plans = 0;
  // An empty plan is logged with the rest: seen live, a task ended "no steps ran" and the trace could not say whether the planner had called it complete.
  // An invalid graph (unknown id, cycle) that the planner's own retry did not fix is sent back once more as a fresh plan call,
  // with the error folded into the situation text; it counts against MAX_PLANS like any other replan.
  // ids of every step this run has already finished, and its result text; a patch's new steps may name one of these in needs (already satisfied).
  const doneIds: string[] = [];
  const doneResults = new Map<string, string>();
  const plan = (t: string, doneNow: string[] = doneIds): Promise<Plan> => ++plans > maxPlans ? Promise.resolve({ steps: [], doneWhen: [] }) :
    (log(`plan ${plans} -> llm`), deps.plan(t)).then((p) => {
      const steps = normalisePlan(p.subplans ? p.subplans.map((sp) => ({ id: sp.id, needs: sp.needs, instruction: sp.goal, sub: { doneWhen: sp.doneWhen ?? [], steps: sp.steps } })) : p.steps ?? []);
      const err = validatePlan(steps, doneNow);
      if (err) throw new Error(err);
      log(`plan: ${steps.length} ${p.subplans ? "subplans" : "steps"}${p.complete ? ", complete" : ""}`);
      return { ...p, steps };
    }).catch((e) => plans > maxPlans ? { steps: [] as Step[], doneWhen: [] as DoneWhen[] } : plan(`${t}\n\nProblem: the last plan was rejected: ${(e as Error).message}`, doneNow));
  const first: Plan = done.length > 0 ? { steps: [], doneWhen: [] } : await plan(task);
  // The planner's own words end a task that only looks: no reply step, no params call, no judge. Jev gates it: an answer is never taken for a task that changes something.
  // Seen live before this: a first plan of no steps marked complete on a task that had work to do.
  if (first.complete && first.answer && first.steps.length === 0) {
    // The lookups are the evidence, as step results are for a plan that ran: an answer they do not bear out is not taken, and the ordinary flow goes on (the planner is asked again).
    const seen = deps.evidence?.() ?? [];
    const v = (await deps.ask({ instruction: taskForJev(task), answer: first.answer, lookups: seen.map((x) => headTail(x, JEV_CHARS / Math.max(1, seen.length))) }, { ...READ_GATE, ...ANSWERED }).catch(() => ({}))) as Record<string, NoulAnswer | undefined>;
    const changes = v.changes?.noul ?? 1, borne = seen.length > 0 ? v.borne?.noul ?? 0 : 0;
    log(`jev: answered from lookups, changes ${changes.toFixed(2)}, borne out ${borne.toFixed(2)}`);
    if (changes < DECIDE && borne >= DECIDE) return `done: ${first.answer}`;
  }
  let { steps, doneWhen } = first;
  doneWhen = dedupeDoneWhen(doneWhen);
  let ran = steps.map((s) => s.instruction);
  let looked = false; // the no-evidence reads ran once
  let unsureAsked = false; // the planner was asked once about a completion Jev was unsure of
  // Done steps go in as their own section, id and result, trimmed like any other Jev/LLM input: never repeat a done step,
  // and a later step may need a done id (treated as already satisfied by the scheduler).
  const doneSection = () => doneIds.length === 0 ? "" : `\n\nAlready done (do not plan these again; a later step may need one of these ids, already satisfied):\n${doneIds.map((id) => `${id}: ${(doneResults.get(id) ?? "").split("\n")[0]}`).join("\n")}`;
  const situation = (why: string, left: Step[]) => `Task:\n${taskForJev(task)}\n\nSteps run so far (each with its output, which may be an error or a refusal):\n${results.map((r) => headTail(r, LLM_CHARS)).join("\n\n") || "nothing"}\n\nRemaining steps:\n${left.map((s) => s.instruction).join("\n") || "none"}${doneSection()}\n\nProblem: ${why}`;

  // One failure is worded once: the ladder's enrich rung, its user rung and escalate all reuse the same draft.
  const drafts = new Map<string, Promise<string>>();
  const draftFor = (why: string, left: Step[]) => { let d = drafts.get(why); if (!d) { log("draft -> llm"); drafts.set(why, d = deps.draft(situation(why, left))); } return d; };
  // Jev is stuck: the LLM words the message, the user decides, and only the user's answer triggers the planner again.
  // Returns a final answer, or undefined after putting the new steps in place.
  // A blocked end says what was tried, so a stop is never silent.
  const tried = () => { const f = verdicts.filter((v) => v.endsWith(": failed")); return f.length ? `\nTried and failed: ${f.map((v) => v.slice(0, -8)).join("; ")}` : ""; };
  async function escalate(why: string, left: Step[], fixed?: string): Promise<string | undefined> {
    // With nobody to ask (a subplan), the failure goes up as it is: no draft is spent on a question never put.
    const message = fixed ?? (deps.askUser && asks++ < MAX_ASKS ? await draftFor(why, left).catch(() => why) : why);
    log(`ask user: ${message.slice(0, 160)}`);
    const answer = (await deps.askUser?.(message))?.trim();
    if (!answer || (fixed && /^(no?|stop|cancel)$/i.test(answer))) return `blocked: ${fixed ? why : message}${tried()}`;
    task = `${task}\n\n[user] ${answer}`;
    const next = await plan(situation(`${why}\nThe user answered: ${answer}\nPlan only what is still left to do.`, left));
    steps = next.steps; if (next.doneWhen.length > 0) doneWhen = dedupeDoneWhen(next.doneWhen);
    return undefined;
  }

  // Returns a final answer when the user stopped the run, true when their lines replaced the remaining steps.
  // `batch`, given when a graph batch is running, lets a step still in flight be voted on (per step, not all-or-nothing)
  // rather than left to run unread: the same rule a failed step's own replan uses.
  async function steered(batch?: { running: Map<string, unknown>; controllers: Map<string, AbortController> }): Promise<string | boolean> {
    const s = deps.steer?.();
    if (s?.stop) { log("stopped by the user"); if (batch) for (const c of batch.controllers.values()) c.abort(); return "blocked: stopped by the user"; }
    if (!s || s.lines.length === 0) return false;
    s.lines.forEach((l) => log(`steered: ${l}`));
    task = `${task}\n\n${s.lines.map((l) => `[user] ${l}`).join("\n")}`;
    const next = await plan(situation(`the user said while this ran: ${s.lines.join(" / ")}\nPlan only what is still left to do.`, steps));
    // The user changed the task, so the old facts may no longer hold.
    steps = next.steps; if (next.doneWhen.length > 0) doneWhen = dedupeDoneWhen(next.doneWhen);
    ran = steps.map((x) => x.instruction);
    if (batch) {
      const kept = new Set(steps.map((x) => x.id));
      for (const rid of batch.running.keys()) if (!kept.has(rid)) batch.controllers.get(rid)?.abort();
    }
    return true;
  }

  // A subplan node runs this same executor one level down: its own planner budget, its own done_when, and a planner call
  // that carries only its goal and the results of the subplans it needs, never the whole task history.
  // ponytail: a dependent sees a subplan as its last result only; pass the inner results out if that proves too thin.
  // ponytail: a line typed mid-run is drained by whichever subplan reads it first and steers that one only.
  const runSub = (s: Step, signal: AbortSignal): Promise<string> => {
    const sub = s.sub!;
    const needed = s.needs.flatMap((n) => doneResults.get(n) ?? []);
    let first = true;
    log(`subplan ${s.id}: ${sub.steps ? `${sub.steps.length} steps` : "planning"}`);
    return runPlan({ ...deps,
      plan: async (t) => {
        if (first && sub.steps) { first = false; return { steps: sub.steps, doneWhen: sub.doneWhen }; }
        first = false;
        const p = await deps.plan(t);
        if (p.subplans) throw new Error("a subplan is planned as steps, not as subplans");
        return { ...p, doneWhen: sub.doneWhen.length > 0 ? sub.doneWhen : p.doneWhen };
      },
      act: (i, r, o) => deps.act(i, r, { ...o, signal: o?.signal ? AbortSignal.any([o.signal, signal]) : signal }),
      steer: () => signal.aborted ? { lines: [], stop: true } : deps.steer?.() ?? { lines: [], stop: false },
      // A subplan never asks the user itself: what it cannot settle goes up as a blocked node, and the top plan decides.
      askUser: undefined,
      log: (t) => log(`  ${s.id}: ${t}`),
    }, `Subplan of a larger task: answer with steps, not subplans.\nGoal: ${s.instruction}${needed.length > 0 ? `\n\nResults of the subplans this one needs:\n${needed.map((r) => headTail(r, LLM_CHARS / needed.length)).join("\n\n")}` : ""}`, [], MAX_SUBPLAN_PLANS);
  };

  for (;;) {
    while (steps.length > 0) {
      const turned = await steered();
      if (typeof turned === "string") return turned;
      if (turned) continue;

      // The graph for this round of steps. status tracks every node the scheduler still owns; a node leaves `steps` (and this
      // map) only once it is done, dropped, or handed back to the planner. Retried is per-step, not global, so one step's
      // single retry does not use up another's.
      type Status = "waiting" | "running" | "done" | "failed" | "interrupted" | "dropped";
      const nodeStatus = new Map<string, Status>(steps.map((s) => [s.id, "waiting"]));
      const byId = new Map<string, Step>(steps.map((s) => [s.id, s]));
      const stepRetried = new Map<string, boolean>();
      const controllers = new Map<string, AbortController>();
      const running = new Map<string, Promise<{ id: string; out: string }>>();
      let final: string | undefined;
      // escalate() and steered() both replan by assigning the outer `steps`; when that happens this batch's own graph is
      // stale (it does not include the new steps), so the batch ends here and the outer while-loop starts a fresh one.
      let restart = false;

      const ready = () => [...byId.values()].filter((s) => nodeStatus.get(s.id) === "waiting" && s.needs.every((n) => nodeStatus.get(n) === "done"));
      const start = (s: Step) => {
        nodeStatus.set(s.id, "running");
        const controller = new AbortController();
        controllers.set(s.id, controller);
        // The context a step sees is the results of done steps only, in plan order, never a step still running alongside it.
        // A rejection becomes a failed result rather than an unhandled rejection: a step's own tool throwing is no
        // different from it returning a bad string, and the settle/judge/failure path already knows what to do with one.
        running.set(s.id, (s.sub ? runSub(s, controller.signal) : deps.act({ instruction: s.instruction, detail: s.detail, command: s.command }, results, { signal: controller.signal })).then((out) => ({ id: s.id, out })).catch((e) => ({ id: s.id, out: `error: ${(e as Error).message ?? e}` })));
      };
      for (const s of ready()) start(s);

      // Runs one settled step through the judge/escalate path used everywhere else; returns a final answer to end the
      // task, or undefined to keep going. On a failure it asks the two graph questions (retry / drop / new steps, and
      // whether to interrupt the rest) through `ladder`, exactly as a step failure was handled before the graph existed.
      async function settle(id: string, out: string): Promise<string | undefined> {
        const step = byId.get(id)!;
        const aborted = controllers.get(id)?.signal.aborted;
        controllers.delete(id);
        running.delete(id);
        if (aborted) {
          // Interrupted on purpose (steer, patch, per-step vote): its output is not a real result, never judged, and
          // never a failure — it just stays not-done, and shows up to the planner with that status like any other.
          nodeStatus.set(id, "interrupted");
          return undefined;
        }
        // Its own slot: a step settling alongside pushes too, so the last result is not always this one.
        const at = results.push(`${step.instruction}\n${out}`) - 1;
        const markDone = (verdict?: string) => { nodeStatus.set(id, "done"); doneIds.push(id); doneResults.set(id, results[at]); if (verdict) verdicts.push(`${step.instruction}: ${verdict}`); return undefined; };
        if (step.sub) {
          // Jev already checked the subplan's done_when inside: done needs no second judge, blocked is a sure failure of this node.
          if (out === "blocked: stopped by the user") return out;
          log(`subplan ${id}: ${out.startsWith("done") ? "done" : "failed"}`);
          if (out.startsWith("done")) return markDone("done");
          verdicts.push(`${step.instruction}: failed`);
          nodeStatus.set(id, "failed");
          return handleFailure(id, `subplan "${step.instruction.slice(0, 120)}" ended ${out.slice(0, 300)}`, true, out);
        }
        if (out.startsWith("denied")) {
          nodeStatus.set(id, "dropped");
          dropDependents(id);
          const f = await escalate(`the user declined this step: ${step.instruction}`, remaining(), `Not approved: "${step.instruction.slice(0, 120)}". Say what to do instead, or "stop".`);
          if (f !== undefined) return f.startsWith("blocked: the user declined") ? `blocked: ${out.slice(0, 300)}` : f;
          restart = true; return undefined; // escalate() replanned into the outer `steps`
        }
        if (out.startsWith("blocked: too deep")) return out.slice(0, 300);
        if (out.startsWith("No tool matches") && remaining().length > 0 && !stepRetried.get(id)) {
          const again = await plan(situation(`no tool does this step: ${step.instruction}\nIf it is an action, plan it as a bash step with the exact shell command in its command field; if it only thinks, drop it. Plan only what is still left to do.`, remaining())).catch(() => ({ steps: [] as Step[], doneWhen: [] as DoneWhen[] }));
          if (again.steps.length > 0) {
            results[at] = `${step.instruction}\nnot run: no tool does this, replanned`;
            nodeStatus.set(id, "dropped"); dropDependents(id);
            applyPatch(again);
            stepRetried.set(id, true); log("replanned (no tool for the step)");
            return undefined;
          }
        }
        if (out.startsWith("No tool matches")) { log("jev: skipped (no tool does this step)"); results[at] = `${step.instruction}\nskipped: no tool does this`; return markDone(); }
        const stops = Object.keys(step.stopOn ?? {});
        let decision = "review", why = `"${step.instruction.slice(0, 120)}" did not give what was needed: ${out.slice(0, 300)}`, sure = false;
        const label = /^\[(\w+)/.exec(out)?.[1] ?? "";
        const ranClean = label === "bash" && !step.expect && out.split("\n").slice(1).join("").trim() === "";
        // A success of these says one fixed thing, so a stopOn outcome has nothing in it to be read from. Seen live: kill_shell stopped the server, the step carried a stopOn, Jev was unsure of "stopped background process p2", and two more plans went looking for a server already gone.
        const fixedOutput = ["tell_user", "kill_shell", "write", "edit"].includes(label);
        if (fixedOutput || (stops.length === 0 && (SELF_EVIDENT.includes(label) || ranClean))) { decision = "next"; sure = true; }
        else try {
          const output = headTail(out);
          const a = await deps.ask({ task: taskForJev(task), step: step.instruction, ...(step.expect ? { expected: step.expect } : {}), output, remaining: remaining().map((s) => s.instruction), ...(stops.length > 0 ? { stop_outcomes: stops } : {}) }, stops.length > 0 ? JUDGE : JUDGE_NO_STOP);
          const d = a.decision as ChoiceAnswer | undefined;
          if (d && d.confidence >= DECIDE) { decision = d.choice; sure = true; } else why = `unsure what this step's output means: ${why}`;
          if (decision === "stop" && stops.length > 1) {
            const w = (await deps.ask({ step: step.instruction, output }, { which: choice("Which outcome does the output show?", Object.fromEntries(stops.map((o, i) => [`o${i}`, o]))) })).which as ChoiceAnswer;
            return `blocked: ${step.stopOn![stops[Number(w.choice.slice(1))] ?? stops[0]]}`;
          }
        } catch (e) { why = `Jev unavailable (${(e as Error).message.slice(0, 80)}): ${why}`; }
        log(`jev: ${decision}${sure ? "" : " (unsure)"}`);
        if (decision === "stop") { if (stops.length > 0) return `blocked: ${step.stopOn![stops[0]]}`; decision = "review"; }
        if (decision === "next") return markDone("done");
        verdicts.push(`${step.instruction}: failed`);
        nodeStatus.set(id, "failed");
        return handleFailure(id, why, sure, out);
      }

      // status graph text for the planner, when a failure needs new steps: every node's current status plus the failure.
      const graphState = () => [...byId.values()].map((s) => `${s.id} [${nodeStatus.get(s.id)}] needs(${s.needs.join(",") || "-"}): ${s.instruction}`).join("\n");
      const remaining = () => [...byId.values()].filter((s) => nodeStatus.get(s.id) === "waiting" || nodeStatus.get(s.id) === "running");
      // Everything that needed the dropped step, transitively, is dropped too and never starts.
      function dropDependents(id: string) {
        let changed = true;
        while (changed) {
          changed = false;
          for (const s of byId.values()) {
            const st = nodeStatus.get(s.id);
            if ((st === "waiting" || st === "running") && s.needs.some((n) => nodeStatus.get(n) === "dropped")) { nodeStatus.set(s.id, "dropped"); changed = true; }
          }
        }
      }
      // A planner patch: new ids are added, an existing not-done id is replaced, optional remove[] drops nodes outright. Done nodes are untouched.
      function applyPatch(p: { steps: Step[]; doneWhen: DoneWhen[]; remove?: string[] }) {
        for (const rid of p.remove ?? []) if (nodeStatus.get(rid) !== "done") { nodeStatus.set(rid, "dropped"); dropDependents(rid); }
        for (const s of p.steps) {
          if (nodeStatus.get(s.id) === "done") continue; // done nodes are never changed or removed
          byId.set(s.id, s); nodeStatus.set(s.id, "waiting");
        }
        if (p.doneWhen.length > 0) doneWhen = dedupeDoneWhen(p.doneWhen);
        for (const s of ready()) if (!running.has(s.id)) start(s);
      }

      // Per running step: is it still worth finishing, given this failure? All the running ids go in one Jev call (one
      // noul question each); confident no aborts that step now, confident yes leaves it, and an unsure one falls to its
      // own ladder (enrich, re-ask, then the user), same as any other unsure verdict.
      async function voteInterrupt(why: string) {
        const runningIds = [...running.keys()];
        if (runningIds.length === 0) return;
        const questions = Object.fromEntries(runningIds.map((rid) => [rid, noul(`Given this failure, is the step "${byId.get(rid)!.instruction}", still running, still worth finishing?`)]));
        let a: Record<string, Answer> = {};
        try { a = await deps.ask({ task: taskForJev(task), why }, questions); } catch { /* every vote falls to its own ladder below */ }
        for (const rid of runningIds) {
          const v = a[rid] as NoulAnswer | undefined;
          if (v && v.noul <= 1 - DECIDE) { controllers.get(rid)?.abort(); continue; }
          if (v && v.noul >= DECIDE) continue;
          const { answer } = await ladder(deps.ask, { task: taskForJev(task), why, step: byId.get(rid)!.instruction }, rid, questions[rid],
            async () => (asks < MAX_ASKS ? draftFor(why, remaining()).catch(() => undefined) : undefined),
            deps.askUser ? async (): Promise<Answer> => {
              asks++;
              const ans = (await deps.askUser!(`Still worth finishing "${byId.get(rid)!.instruction}" given ${why}? [y/N]`))?.trim().toLowerCase();
              return { type: "noul", noul: ans?.startsWith("y") ? 1 : 0 };
            } : undefined);
          if ((answer as NoulAnswer).noul <= 1 - DECIDE) controllers.get(rid)?.abort();
        }
      }

      // A step's judge verdict of failure/unsure asks two Jev questions through `ladder`, the same idiom `severalLadder` and
      // the `write` tool use: enrich with the existing planner-ask (or the existing think path is not available here, so the
      // planner IS the enrich) and fall back to the user, bounded by MAX_ASKS.
      async function handleFailure(id: string, why: string, sure: boolean, out: string): Promise<string | undefined> {
        const step = byId.get(id)!;
        const graphQ = choice("What should happen with this failed step in the graph?", {
          retry: "Run the exact same step again; the output may have been a fluke or a transient error",
          drop: "Drop this step and everything that depends on it; it and its dependents are not needed for the task",
          replan: "The plan needs new or different steps to get past this: call the planner with the graph and this failure",
        });
        const state = { task: taskForJev(task), step: step.instruction, output: headTail(out), why, graph: graphState() };
        const canRetry = !stepRetried.get(id);
        log(`failure: ${why.split("\n")[0].slice(0, 160)}`);
        // An unsure vote goes to the planner, which can look and find a way past; the user hears of it only when the planner has nothing left.
        const { answer, sure: voted } = await ladder(deps.ask, state, "graph", graphQ,
          async () => (asks < MAX_ASKS ? draftFor(why, remaining()).catch(() => undefined) : undefined));
        const picked = (answer as ChoiceAnswer | undefined)?.choice;
        const choiceMade = !voted || !picked || (picked === "retry" && !canRetry) ? "replan" : picked;
        log(`jev: graph ${choiceMade}`);
        if (choiceMade === "retry") { stepRetried.set(id, true); nodeStatus.set(id, "waiting"); start(step); return undefined; }
        if (choiceMade === "drop") { nodeStatus.set(id, "dropped"); dropDependents(id); return undefined; }
        // needs new steps: a per-step vote, not all-or-nothing, on whether each still-running step is worth finishing
        // given this failure.
        await voteInterrupt(why);
        const before = JSON.stringify([...byId.keys()]);
        const again = await plan(`${situation(`this step went wrong: ${why}\nA failure is something to work out and get past, not a reason to stop. Go by what the output shows: read a file it names before changing anything, and fix the cause it shows, not a guess. A step that already failed is never planned again unchanged: when a fix was tried and the same failure came back, take another route (another file, another command, another tool).\nPlan only the steps still needed, as a patch: new ids are added, an existing not-done id is replaced. Plan only what is still left to do, using what the steps found; name exact files. A failure in what these steps changed (the edited file, its callers, its tests) is this plan's to fix: when unsure whether it was there before, treat it as caused by the change. Only a failure plainly apart from the change is left alone: the steps still not run stay in the plan, the failing command is not run again, the plan ends with a tell_user step reporting the failure, and done_when no longer says that command passes.`, remaining())}\n\nCurrent graph:\n${graphState()}`).catch(() => ({ steps: [] as Step[], doneWhen: [] as DoneWhen[] }));
        if (again.steps.length > 0 && JSON.stringify(again.steps.map((s) => s.id)) !== before) {
          // Exact rule, no vote: any running step whose node the patch removed or replaced is aborted, since it is no longer part of the graph.
          const touched = new Set([...(again as { remove?: string[] }).remove ?? [], ...again.steps.map((s) => s.id)]);
          for (const rid of running.keys()) if (touched.has(rid)) controllers.get(rid)?.abort();
          applyPatch(again);
          return undefined;
        }
        if (!sure && again.steps.length > 0 && !/\[exit code \d+\]/.test(out)) { log("unsure, and the planner kept the plan: going on"); verdicts[verdicts.lastIndexOf(`${step.instruction}: failed`)] = `${step.instruction}: unclear`; nodeStatus.set(id, "done"); doneIds.push(id); doneResults.set(id, `${step.instruction}\n${out}`); return undefined; }
        // A subplan out of planner calls hands its failure up, so the top plan is patched; it never reports itself done.
        if (plans > maxPlans && maxPlans < MAX_PLANS) return `blocked: ${why}`;
        if (remaining().length === 0 && again.steps.length === 0) { log("nothing left to plan: reporting"); return `done: ${await deps.draft(situation(`${why}\nNothing is left to do. Tell the user what was done and what still fails. Do not ask a question.`, [])).catch(() => why)}`; }
        const f = await escalate(why, remaining());
        if (f !== undefined) return f;
        restart = true; return undefined; // escalate() replanned into the outer `steps`
      }

      // The event loop for this batch: wait for the next settling step, judge it, start whatever became ready.
      while (final === undefined && !restart && running.size > 0) {
        const { id, out } = await Promise.race(running.values());
        const result = await settle(id, out);
        if (result !== undefined) { final = result; break; }
        if (restart) break;
        // Check for a steer/stop before starting anything newly ready: a step that only became ready
        // this tick must not slip in ahead of a line typed mid-run.
        const t = await steered({ running, controllers });
        if (typeof t === "string") { final = t; break; }
        if (t) { restart = true; break; } // steered() replanned into the outer `steps`
        for (const s of ready()) if (!running.has(s.id)) start(s);
      }
      if (final !== undefined) { for (const c of controllers.values()) c.abort(); return final; }
      if (restart) {
        // outer `steps` already holds the new plan; abort every step this batch still has in flight and wait for
        // them to settle before starting the fresh batch, so none is left to finish unread and race a replanned
        // step of the same id, and no late rejection escapes unhandled.
        for (const c of controllers.values()) c.abort();
        await Promise.allSettled(running.values());
        continue;
      }

      // Nodes left waiting with no way to become ready (their needs were dropped or failed) are a stuck graph: treat as needs-new-steps.
      const stuck = [...byId.values()].filter((s) => nodeStatus.get(s.id) === "waiting");
      // They are dropped, no planner call: the completion check below calls the planner only when the facts are not yet true.
      if (stuck.length > 0) log(`dropped, their needs never finished: ${stuck.map((x) => x.id).join(", ")}`);
      steps = [];
    }
    // A line typed during the last step is seen before the task is called complete.
    const turned = await steered();
    if (typeof turned === "string") return turned;
    if (turned && steps.length > 0) continue;
    // Jev judges completion: one criterion per done_when fact, or the generic question when the plan carried none.
    // With nothing run, the planner is told so. Seen live: a first plan of no steps marked complete, the review told "the steps ran", no steps again, and the user asked.
    let complete = true, why = results.length === 0 ? "the plan had no steps: nothing has run and nothing was changed. Plan the steps that do the task" : "the steps ran but the task may not be complete", notYet = "";
    let failedCriteria: DoneWhen[] = [];
    let contradicted: DoneWhen[] = []; // facts naming a file, scored low from a result about that very file
    if (doneWhen.length === 0) {
      let score = 1;
      try { score = ((await deps.ask({ task: taskForJev(task), results: results.map((r) => headTail(r, JEV_CHARS / results.length)) }, COMPLETE)).complete as NoulAnswer).noul; } catch { score = 0; /* no verdict is not a yes: it goes the way of a low score */ }
      log(`jev: complete ${score.toFixed(2)}`);
      // An unsure "complete" goes to the planner once before it is taken. Seen live: "the count looks wrong" got a one-step plan that ran the tests, 0.56, Done with the bug unfixed.
      complete = score >= ACT || (score >= DECIDE && unsureAsked);
      if (!complete && score >= DECIDE) { unsureAsked = true; notYet = `Not yet: it is unclear that the task is complete (${score.toFixed(2)})`; }
    } else {
      // Per criterion: one naming files is evidenced only by results whose label mentions one of them, matched on a segment
      // boundary so "todos.js" does not pick up "legacy/old-todos.js". No such result means no evidence and it is never
      // asked to Jev. A criterion naming no files keeps the last-result fallback.
      const evidenceFor = (d: DoneWhen): { files: string[]; evidence: string[] } => {
        const files = d.files ?? [];
        if (files.length === 0) {
          // A fact naming no file is about a command ("bun test passes"): the last run speaks to it, unless a file changed since, and then nothing does.
          // Seen live: tests failed, the test file was edited, and the edit's own result scored "bun test passes" at 0.61: Done with 6 failing.
          const labels = results.map((r) => /^\[(\w+)/.exec(r.split("\n")[1] ?? "")?.[1] ?? "");
          const lastRun = labels.lastIndexOf("bash");
          if (lastRun < 0) return { files, evidence: results.slice(-1) };
          const stale = labels.slice(lastRun + 1).some((l) => l === "write" || l === "edit");
          return { files: stale ? ["a run since the last change"] : files, evidence: stale ? [] : [results[lastRun]] };
        }
        const evidence = results.filter((r) => {
          // The instruction and the tool label under it: a step worded without the file name still ran on it.
          const tokens = r.split("\n").slice(0, 2).join(" ").split(/[\s=\]\[`"',]/);
          // What the user was told speaks to any fact. Seen live: "the user is told ... handlers_test.go was reported" had no evidence, its label naming no file, and the task ended blocked.
          return TALK.includes(/^\[(\w+)/.exec(r.split("\n")[1] ?? "")?.[1] ?? "") || files.some((f) => tokens.some((t) => t === f || t.endsWith(`/${f}`)));
        });
        return { files, evidence };
      };
      const perCriterion = doneWhen.map((d, index) => ({ d, index, ...evidenceFor(d) }));
      const withEvidence = perCriterion.filter((p) => p.evidence.length > 0 || p.files.length === 0);
      const scores: number[] = doneWhen.map(() => 0);
      if (withEvidence.length > 0) {
        const questions = Object.fromEntries(withEvidence.map((p, i) => [`c${i}`, noul(`Is this true now: ${p.d.fact}?`)]));
        const share = JEV_CHARS / Math.max(1, withEvidence.reduce((n, p) => n + p.evidence.length, 0));
        const state = { task: taskForJev(task), steps: verdicts, evidence: Object.fromEntries(withEvidence.map((p, i) => [`c${i}`, p.evidence.map((e) => headTail(e, share))])) };
        try {
          const a = await deps.ask(state, questions);
          withEvidence.forEach((p, i) => { scores[p.index] = (a[`c${i}`] as NoulAnswer | undefined)?.noul ?? 0; });
        } catch { /* no verdict: each criterion counts as failed */ }
      }
      const noEvidence = new Set(perCriterion.filter((p) => !withEvidence.includes(p)).map((p) => p.d));
      doneWhen.forEach((d, i) => log(`jev: done_when "${d.fact}" ${noEvidence.has(d) ? "no evidence" : scores[i].toFixed(2)}`));
      const failed = doneWhen.filter((d, i) => noEvidence.has(d) || scores[i] < DECIDE);
      failedCriteria = failed;
      contradicted = perCriterion.filter((p) => p.files.length > 0 && p.evidence.length > 0 && scores[p.index] < DECIDE).map((p) => p.d);
      complete = failed.length === 0;
      if (!complete) {
        why = `these could not be confirmed: ${failed.map((d) => d.fact).join("; ")}`;
        // One "Not yet" line per failed criterion, so the planner plans only for those.
        notYet = doneWhen.map((d, i) => (noEvidence.has(d) ? `Not yet: ${d.fact} (no evidence)` : scores[i] < DECIDE ? `Not yet: ${d.fact} (${scores[i].toFixed(2)})` : "")).filter(Boolean).join("\n");
      }
    }
    if (complete) break;
    if (asks >= MAX_ASKS) return `blocked: ${why}`;
    // The plan ran out with nothing gone wrong: it was an exploring plan (list the files, then decide), or done_when named facts still not shown.
    // The replan following a failed completion check is a review: it may find the facts already true and close the task with no more steps.
    // A fact no step output spoke to, naming a file that is there: read it (Jev only) and check again before any planner call.
    const unread = looked ? [] : [...new Set(failedCriteria.flatMap((d) => d.files ?? []))].filter((f) => deps.exists?.(f));
    if (unread.length > 0) { looked = true; log(`not confirmed yet, reading ${unread.join(", ")}`); steps = unread.map((f, i) => ({ id: `_r${i}`, needs: [], instruction: `read ${f}` })); ran = steps.map((s) => s.instruction); continue; }
    const before = JSON.stringify(ran);
    const next = await plan(situation(notYet || why, []));
    // The review may vouch only for facts no step output spoke to (it looked; Jev had nothing, or only the last result to go by). A fact Jev scored low from real
    // evidence stands against the LLM's word: "todos.js no longer exists" at 0.32 after a "wrote todos.js" is not complete.
    if (notYet && next.complete && contradicted.length > 0) { const facts = contradicted.map((d) => d.fact); log(`review: complete refused, evidence says otherwise: ${facts.join("; ")}`); return `blocked: these could not be confirmed: ${facts.join("; ")}`; }
    // The review's word is not proof for a fact no step output spoke to. Seen live: four "no evidence" facts, "review: complete", "Done.", and a broken
    // project. Each named file is read once (Jev only, no LLM call) and the check runs again on what the reads show.
    const unseen = notYet && next.complete && !looked ? [...new Set(failedCriteria.flatMap((d) => d.files ?? []))] : [];
    if (unseen.length > 0) { looked = true; log(`review: complete not taken on trust, reading ${unseen.join(", ")}`); steps = unseen.map((f, i) => ({ id: `_r${i}`, needs: [], instruction: `read ${f}` })); ran = steps.map((s) => s.instruction); continue; }
    if (notYet && next.complete && next.steps.length === 0) { log("review: complete"); return `done: ${next.answer ?? results.at(-1)?.split("\n").slice(1).filter((l, i) => i > 0 || !l.startsWith("[")).join("\n").replace(/^done: /, "") ?? ""}`; }
    if (notYet && next.complete && next.steps.length > 0) log("review: complete ignored, steps returned");
    // A replan's own doneWhen replaces the list, but a criterion Jev just found failing must not be dropped from the next check.
    steps = next.steps; if (next.doneWhen.length > 0) doneWhen = dedupeDoneWhen([...failedCriteria, ...next.doneWhen]).slice(0, MAX_CRITERIA);
    ran = steps.map((s) => s.instruction);
    if (steps.length > 0 && JSON.stringify(ran) !== before) continue;
    const final = await escalate(why, []);
    if (final !== undefined) return final;
    if (steps.length === 0) return `blocked: ${why}`;
  }
  return `done: ${results.at(-1)?.split("\n").slice(1).filter((l, i) => i > 0 || !l.startsWith("[")).join("\n").replace(/^done: /, "") ?? ""}`;
}

export type RunTaskDeps = { llm: Llm; ask: Ask; cwd: string; steer?: () => { lines: string[]; stop: boolean }; log: (text: string) => void; approve?: (tool: string, args: Record<string, string>, destructive: boolean) => Promise<boolean>; user?: User; context: () => string; tools?: Tool[]; readOnly?: boolean };

// Carries out a typed line as a task: plans it, runs each step through Jev-gated tool calls, and drafts a message
// to the user for whatever Jev cannot settle. `done` holds a tool call already run for this line (main's own direct
// call, before it was judged incomplete), so the plan continues from it instead of starting over.
export function runTask(deps: RunTaskDeps, line: string, title: string, done: string[] = []): Promise<string> {
  const actDeps = { llm: deps.llm, ask: deps.ask, cwd: deps.cwd, log: deps.log, approve: deps.approve, user: deps.user, tools: deps.tools, readOnly: deps.readOnly };
  // context() gives every closed section as a label+summary and the open section in full, uncut: the same view a
  // fresh task and a continued one both start from, since there is no separate running session to carry it forward.
  const task = `${deps.context()}\n\n${line}`;
  const looks: Looks = { n: 0, seen: [] }; // shared across every look tool made for this task, so MAX_LOOKS does not reset on replan
  for (const d of done) remember(looks, d.slice(d.indexOf("\n") + 1));
  const look = () => makeLook({ llm: deps.llm, cwd: deps.cwd, ask: deps.ask, log: actDeps.log, approve: actDeps.approve }, looks);
  const plans = makePlanner(deps.llm, deps.cwd, look), thinks = makeDrafter(deps.llm, deps.cwd, THINK_SYSTEM, look);
  const planner: Planner = (t) => plans(t + seenText(looks)), thinker: Drafter = (t) => thinks(t + seenText(looks));
  const drafter = makeDrafter(deps.llm, deps.cwd);
  // No budget: Jev checks each decision against the task, and one that moves away from it goes to the user.
  const think = async (q: string) => {
    const decision = await thinker(`Task: ${task}\n\n${q}`);
    const a = await deps.ask({ task, question: q, decision }, { on_plan: noul("Does the decision stay within the task and serve its plan, rather than change the goal, widen the scope or start different work?") });
    if ((a.on_plan as NoulAnswer).noul >= DECIDE) return decision;
    // Off the plan: the user settles it. Their answer is the decision; with no user wired the step is blocked.
    if (!actDeps.user) throw new Error(`blocked: the thinker moved away from the task: ${decision.slice(0, 200)}`);
    return `The user decided: ${await actDeps.user.ask(`While deciding "${q.slice(0, 200)}" I landed on something outside the task:\n${decision}\nGo with it, or tell me what to do instead?`)}`;
  };
  const base = root(title, { task: title }, RESPONSES);
  // Each step's params are built with the earlier steps' outputs in view.
  const act = (input: ActInput, results: string[], opts?: ActOpts) => makeAct({ ...actDeps, generate: makeGenerate({ ...actDeps, looks }), think }, () => ({}), () => child(base, input.instruction, { earlierSteps: recentSteps(results) }, RESPONSES, {}, { earlierSteps: recentSteps(results, JEV_FRAME_CAP - 600, JEV_FRAME_CAP - 600) }))(input, results, opts);
  return runPlan({
    plan: planner, draft: drafter, ask: deps.ask, askUser: actDeps.user?.ask, log: deps.log, steer: deps.steer, exists: (f) => existsSync(join(deps.cwd, f)), evidence: () => looks.seen,
    act: async (input, results, opts) => { const out = await act(input, results, opts); forget(looks, out); deps.log(out); return out; },
  }, task, done);
}
