import type { AgentState, ServiceState, TodoState } from "../model";

/**
 * One place where a state becomes a colour, a dot and a label. Every list that shows a
 * state reads from here, so an agent, a service and a plan step that are all
 * "running" look the same wherever they appear. In dense lists the state is a
 * plain dot and the word lives in the tooltip; only the plan, which has room,
 * uses the glyph.
 */
export type Status = { icon: string; tone: string; dot: string; label: string };

export const AGENT: Record<AgentState, Status> = {
  running: { icon: "dotFilled", tone: "text-success", dot: "bg-success", label: "running" },
  waiting: { icon: "clock", tone: "text-warning", dot: "bg-warning", label: "waiting" },
  done: { icon: "check", tone: "text-muted", dot: "bg-subtle", label: "done" },
  failed: { icon: "x", tone: "text-danger", dot: "bg-danger", label: "failed" },
  idle: { icon: "dot", tone: "text-subtle", dot: "bg-subtle", label: "idle" },
};

export const SERVICE: Record<ServiceState, Status> = {
  running: { icon: "dotFilled", tone: "text-success", dot: "bg-success", label: "running" },
  starting: { icon: "clock", tone: "text-warning", dot: "bg-warning", label: "starting" },
  stopped: { icon: "dot", tone: "text-subtle", dot: "bg-subtle", label: "stopped" },
  failed: { icon: "x", tone: "text-danger", dot: "bg-danger", label: "failed" },
};

export const TODO: Record<TodoState, Status> = {
  done: { icon: "check", tone: "text-muted", dot: "bg-subtle", label: "done" },
  active: { icon: "dotFilled", tone: "text-success", dot: "bg-success", label: "in hand" },
  blocked: { icon: "clock", tone: "text-warning", dot: "bg-warning", label: "blocked" },
  pending: { icon: "dot", tone: "text-subtle", dot: "bg-subtle", label: "not started" },
};

export const ROLE_SHORT: Record<string, string> = { implementer: "impl", reviewer: "review", planner: "plan" };
