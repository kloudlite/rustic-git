import { choice, type Answer, type ChoiceAnswer, type Question } from "./jev.ts";
import { GENERIC_MESSAGES, fillTemplate } from "./task.ts";

export const REPLY_ASK = 0.5;

export const REPLIES = {
  failed: (e: string) => fillTemplate(GENERIC_MESSAGES.failed, { detail: e }),
  queued: "Queued behind the current task.",
  cancelled: "Cancelled.",
  noted: "Noted.",
  working: "Working on it.",
  chat: GENERIC_MESSAGES.chat,
};

export function replyQuestion(): Question {
  return choice("How should the main session reply to the instruction?", {
    // Seen live: "can we move from npm to pnpm" was read as a question and only got an explanation. A polite request is still a request.
    work: "The instruction needs something done: a tool run, code, or a multi-step task. This includes a request phrased as a question: 'can we / could you / shall we / why don't we' followed by a change to make (move, add, switch, fix, remove) means do it",
    noted: "The user only gave information or a preference; nothing to run",
    chat: "Only a bare greeting, thanks or goodbye; it contains no question and no request. A line that asks anything, however small, is never this",
    say: "The user only wants to know something (what, why, how does, where is, is it possible) about the project or the assistant itself and asks for no change, or wants an answer none of the fixed replies can express",
  });
}

export type Reply = "work" | "noted" | "chat" | "say" | "unsure";

// "unsure" never starts a task by itself: the caller runs a confidently chosen tool, else asks main's LLM.
export function pickReply(answers: Record<string, Answer>): Reply {
  const a = answers.reply as ChoiceAnswer | undefined;
  if (!a || a.confidence < REPLY_ASK) return "unsure";
  return (a.choice as Reply) ?? "unsure";
}

export function outcomeQuestion(outcomes: string[]): Question {
  return choice("Which outcome best matches the tool's output?", {
    ...Object.fromEntries(outcomes.map((o) => [o, o])),
    other: "None of the listed outcomes fit",
  });
}

export type Outcome = { outcome: string; confident: boolean };

// Below REPLY_ASK the caller falls back to showing the raw output, same threshold as pickReply.
export function classifyOutcome(answers: Record<string, Answer>): Outcome {
  const a = answers.outcome as ChoiceAnswer | undefined;
  if (!a || a.confidence < REPLY_ASK) return { outcome: "other", confident: false };
  return { outcome: a.choice, confident: true };
}
