import { choice, noul, type Answer, type Ask, type ChoiceAnswer, type NoulAnswer, type Question } from "./jev.ts";
import { TOOLS, ACT, type Tool } from "./tools.ts";

export { ACT };
export const ASK = 0.5;

// Confident by Jev's own rule for the question's shape: a choice needs ACT either way, a noul needs ACT or its mirror at the low end.
function confident(a: Answer): boolean {
  return a.type === "choice" ? a.confidence >= ACT : a.noul >= ACT || a.noul <= 1 - ACT;
}

// The owner's rule for one question, in order: ask Jev; confident, done. Not confident, and enrich() is given: call it for
// extra context, add it to the state under "context", ask Jev again. Still not confident, and user() is given: use its answer.
// Neither rung available (or both skipped): the last Jev answer is returned, marked unsure.
export async function ladder(
  ask: Ask, state: Record<string, unknown>, name: string, question: Question,
  enrich?: () => Promise<string | undefined>, user?: () => Promise<Answer>,
): Promise<{ answer: Answer; sure: boolean }> {
  let answer = (await ask(state, { [name]: question }))[name];
  if (confident(answer)) return { answer, sure: true };
  if (enrich) {
    const context = await enrich();
    if (context !== undefined) {
      answer = (await ask({ ...state, context }, { [name]: question }))[name];
      if (confident(answer)) return { answer, sure: true };
    }
  }
  if (user) return { answer: await user(), sure: true };
  return { answer, sure: false };
}
// Live scores: real one-command steps 0.74 to 0.82, compound or thinking steps 0.06 and below. bash always asks, so the bar can sit between.
const SHELL = 0.7;
const NONE_SPLIT = 0.2;

// Arbitrary shell is not contained to cwd, so it is never run without the user's say-so.
export const ALWAYS_CONFIRM = ["bash", "kill_shell"];

export type Decision = { kind: "run" | "confirm"; tool: Tool; destructive: boolean; confidence?: number; soft?: boolean } | { kind: "none"; confidence?: number };

// Always the same bytes: Jev warms per question block (a new block costs ~1.2s, a seen one ~0.35s), so nothing per call goes in here; what varies goes in the state.
export function chooserQuestions(): Record<string, Question> {
  return {
    tool: choice("Which single tool carries out the instruction?", {
      ...Object.fromEntries(TOOLS.map((t) => [t.name, t.description])),
      none: "No single tool does this: it is several steps at once (restart a server = stop it, then start it; fix something, then check it), or a question that needs thinking rather than an action",
    }),
    // The shell is always there when no tool fits. Narrow on purpose: a compound step or a decision must stay "none" and go back to the planner.
    shell: noul("Can exactly one shell command carry out the whole instruction, with nothing left to decide or think through?"),
    destructive: noul("Would carrying out the instruction delete data, discard uncommitted work, or force-push?"),
  };
}

export function decide(answers: Record<string, Answer>): Decision {
  const t = answers.tool as ChoiceAnswer;
  const tool = TOOLS.find((x) => x.name === t.choice);
  // Low confidence with "none" barely in the running is a split between two real tools (bash_output or bash), not a missing tool: the top one is confirmed.
  const destructive = (answers.destructive as NoulAnswer).noul >= 0.5;
  if (!tool || (t.confidence < ASK && (t.probabilities?.none ?? 1) > NONE_SPLIT)) {
    // Last resort: no tool fits, but one shell command does it. bash always asks first, so a wrong call costs one "no".
    const bash = TOOLS.find((x) => x.name === "bash");
    // A weak top pick of bash is still bash: seen live, "create a branch feature/health" scored bash 0.45, shell 0.13, was skipped, and the commit landed on master.
    if (bash && (tool === bash || ((answers.shell as NoulAnswer | undefined)?.noul ?? 0) >= SHELL)) return { kind: "confirm", tool: bash, destructive, confidence: t.confidence };
    return { kind: "none", confidence: t.confidence };
  }
  // Doubt about the pick only matters for a tool that changes something: a wrong read or listing costs nothing, and asking "read package.json?" at 0.88 was pure noise.
  const changes = ALWAYS_CONFIRM.includes(tool.name) || tool.name === "write" || tool.name === "edit";
  if (destructive || (changes && t.confidence < ACT) || ALWAYS_CONFIRM.includes(tool.name)) return { kind: "confirm", tool, destructive, confidence: t.confidence, soft: !destructive && t.confidence >= ACT };
  return { kind: "run", tool, destructive, confidence: t.confidence };
}
