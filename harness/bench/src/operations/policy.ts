import type { OperationErrorCode } from "./contracts.ts";

/** What every tool answers with, here and in `workspace-tools.ts`. */
export type ToolResult = { content: { type: "text"; text: string }[]; isError?: boolean };

/**
 * How a dispatch ended. `refused` means policy stopped the call before the
 * handler ran, so no effect exists to reconcile and the executor must not read
 * it as success. A handler that ran and answered with an error is `failed`.
 */
export type DispatchOutcome<T = ToolResult> =
  | { outcome: "completed"; result: T }
  | { outcome: "failed"; code: OperationErrorCode; result: T }
  | { outcome: "refused"; code: OperationErrorCode; reason: string };

export type ApprovalRequirement = "none" | "user" | "policy";

export const DECLINED = "declined by the person";
const NO_APPROVAL_CHANNEL = "that change needs approval and no approval channel is available; nothing ran";

/**
 * One policy-bearing dispatch for every caller: the registered tools (a person
 * is asked asynchronously through `propose`) and the operation executor's
 * capabilities. Scope/path/argument refusals run first, then approval, then the
 * handler — in that order, so a call that cannot be authorized never reaches it,
 * and a mutating plan with no approval channel is refused rather than run.
 */
export type PolicyPlan<T = ToolResult> = {
  capability: string;
  effect: "read" | "write" | "destroy";
  approval: { required: ApprovalRequirement; obtain?: () => Promise<boolean> };
  inspect?: () => { code: OperationErrorCode; reason: string } | undefined;
  run: () => Promise<T>;
  failed?: (result: T) => OperationErrorCode | undefined;
  /** The answer a thrown handler error becomes; the registered tools keep `thrown`'s sentence. */
  error?: (e: unknown) => ToolResult;
};

export async function dispatchWithPolicy<T = ToolResult>(plan: PolicyPlan<T>): Promise<DispatchOutcome<T>> {
  const blocked = plan.inspect?.();
  if (blocked) return { outcome: "refused", code: blocked.code, reason: blocked.reason };
  if (plan.approval.required !== "none") {
    const obtain = plan.approval.obtain;
    if (!obtain) return { outcome: "refused", code: "permission_denied", reason: NO_APPROVAL_CHANNEL };
    const granted = await obtain().catch((error) => {
      if (plan.capability.includes(".")) throw error;
      return false;
    });
    if (!granted) return { outcome: "refused", code: "permission_denied", reason: DECLINED };
  }
  try {
    const result = await plan.run();
    const code = plan.failed?.(result) ?? ((result as ToolResult)?.isError ? "execution_failure" : undefined);
    return code ? { outcome: "failed", code, result } : { outcome: "completed", result };
  } catch (e) {
    if (!plan.error) throw e;
    const answer = plan.error(e);
    return { outcome: "failed", code: "execution_failure", result: { ...answer, isError: true } as T };
  }
}

/**
 * The registered tool surface keeps the words the model already reads: a
 * decline is its own sentence, a scope/argument refusal is an error, and an
 * error the handler answered with is unchanged. The executor reads the
 * `DispatchOutcome` itself, where a denial is never "completed".
 */
export function toolResultOf(outcome: DispatchOutcome<ToolResult>, declined: "error" | "plain" = "error"): ToolResult {
  if (outcome.outcome === "completed") return outcome.result;
  if (outcome.outcome === "failed") return { ...outcome.result, isError: true };
  if (declined === "plain" && outcome.code === "permission_denied" && outcome.reason === DECLINED) {
    return { content: [{ type: "text", text: outcome.reason }] };
  }
  return { content: [{ type: "text", text: outcome.reason }], isError: true };
}

/** The six skills, read from beside the extension: product words, not tool lists. */
export const SKILLS = ["workspaces", "environments", "snapshots", "repos", "images", "agents"];

/**
 * A bench session no longer defaults ANY tool to its own machine (spec §3.1: it has none). A
 * workspace session still defaults to its own workspace, which is what `ownTools` is.
 */
export const NOT_A_WORKSPACE = "that is you, not a workspace; name a workspace";
export const isOwnBench = (id: string): boolean => {
  const own = process.env.KL_WORKSPACE_ID;
  // The bench's own workspace id, and the `bench-` objects the platform names a bench with
  // (`crd::bench_id`) — a listing never offers one, so anything shaped like one is a mistake.
  return !!id && (id === own || /^bench-[0-9a-f]{8,}$/.test(id));
};
