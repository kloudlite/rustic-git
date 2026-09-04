import type { Tone } from "@/lib/console";
import type { AuditEntry } from "@/lib/audit";

/** A refusal is an EVENT, not an absence (design README): a 409 create and an admission deny are
 *  the rows an operator came to this page for. 409 is amber — the system worked, a limit held —
 *  while 403 and 5xx are red.
 *
 *  The wire carries the outcome as `result`, one open string (`"ok"`, `"error: 409"`), not a
 *  numeric status, so the code is read back out of it rather than invented beside it. */
export function resultPill(entry: AuditEntry): { tone: Tone; label: string } {
  const label = entry.result?.trim() || "ok";
  if (!label.startsWith("error")) return { tone: "ok", label };
  const status = Number(label.match(/\d{3}/)?.[0]);
  return { tone: status === 409 ? "warn" : "critical", label };
}

/** A row the operator is on this page to find: anything the api refused. */
export function isRefusal(entry: AuditEntry): boolean {
  return resultPill(entry).tone !== "ok";
}
