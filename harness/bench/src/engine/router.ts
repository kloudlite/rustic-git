import { choice, noul, type Answer, type Ask, type ChoiceAnswer, type NoulAnswer, type Question } from "./jev.ts";
import type { Decision } from "./chooser.ts";
import { ladder } from "./chooser.ts";
import { ACT, type Tool } from "./tools.ts";

export type Route = { step: "run" | "confirm"; tool: Tool; destructive: boolean; soft?: boolean } | { step: "escalate" };

// A tool pick alone ran half of "move the todos to a separate file": it wrote the new file and called it done. Work of several steps becomes a task, which plans it.
export const SEVERAL = noul("Does carrying out this instruction take more than one tool call (change more than one file, move or rename code between files, do something and then check it)?");

export function routeQuestions(hasOpenMessages = false): Record<string, Question> {
  const q: Record<string, Question> = { several: SEVERAL };
  // "show me login.js" wants the file; "show me all the routes" wants a list drawn from it, which only an LLM can write.
  q.answer = noul("Does the user want an answer worked out from what is looked up (a list, a location, an explanation, a value), rather than the file or output itself put on screen?");
  // Asked only when the open section already has messages: with nothing open yet, there is no topic to compare against.
  if (hasOpenMessages) {
    q.topic = choice("How does the new instruction relate to the recent messages?", {
      follow_up: "It builds on, refers to or asks about the work in the recent messages",
      new_topic: "It is about something else: other files, another feature, another subject",
    });
  }
  return q;
}

export function during(): Question {
  return choice("A task is running. What is the new instruction to it?", {
    steer: "It changes how the running task should go on: a correction, a different value, a step to skip or add",
    queue: "New work to start after the running task ends: a further task, often opening with then, next, after that, once done, also",
    stop: "It cancels the running task (stop, cancel, never mind)",
  });
}

// Jev unsure whether the line is one tool call or several: a read-only lookup for more context, then Jev again.
// Still unsure: the planner takes the line. It can write a graph of one step, so that is never wrong; the user is not
// asked, because how many tool calls a line takes is not something they can know.
export async function severalLadder(ask: Ask, state: Record<string, unknown>, answers: Record<string, Answer>, enrich: () => Promise<string | undefined>): Promise<void> {
  const { answer, sure } = await ladder(ask, state, "several", SEVERAL, enrich);
  answers.several = sure ? answer : { type: "noul", noul: 1 };
}

export function route(answers: Record<string, Answer>, decision: Decision): Route {
  if (decision.kind === "none" || ((answers.several as NoulAnswer | undefined)?.noul ?? 1) >= 0.5) return { step: "escalate" };
  return { step: decision.kind, tool: decision.tool, destructive: decision.destructive, ...(decision.kind === "confirm" && decision.soft ? { soft: true } : {}) };
}

export type During = "steer" | "queue" | "stop" | "ask";
// Below ACT the user is asked: a wrong "steer" throws away the plan's remaining steps, a wrong "stop" the whole run.
export function pickDuring(answers: Record<string, Answer>): During {
  const d = answers.during as ChoiceAnswer | undefined;
  if (!d) return "ask"; // only called while a task runs: with no verdict the user says what the line is
  return d.confidence >= ACT ? d.choice as During : "ask";
}

// Only tools with exactly one free parameter can be driven by the user's literal text.
export function freeFromLiterals(tool: Tool, instruction: string, cwd?: string): Record<string, string> {
  // A backticked literal is never file content: "add `/health`" must not write "/health" over the file.
  const free = tool.params.filter((p) => p.kind === "free" && p.literal !== false);
  const literal = instruction.match(/`([^`]+)`/)?.[1];
  if (free.length !== 1 || !literal) return {};
  // Seen live: "start the server with `node index.js` in sample-node-app" ran the literal in the project root and failed. A directory named
  // outside the backticks is where the command runs.
  // Directory detection needs the pod (fs is remote now); the model's own `cd` written literally in the instruction still works.
  const isDir = (_w: string) => false;
  const dir = tool.name === "bash" && !/\bcd\s/.test(literal) ? instruction.replace(/`[^`]+`/g, " ").split(/\s+/).map((w) => w.replace(/^["'(]+|["').,:;]+$/g, "")).find((w) => w && w !== "." && isDir(w)) : undefined;
  return { [free[0].name]: dir ? `cd ${dir} && ${literal}` : literal };
}
