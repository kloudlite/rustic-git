import { ShieldCheck, ShieldQuestion } from "lucide-react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { ApiVerification } from "@/lib/api";

/** What a commit's signature amounts to.
 *
 *  An unsigned commit shows NOTHING. Most commits are unsigned, and a badge on
 *  every one of them would train people to ignore the badge — which is the one
 *  thing a verification marker must not do. */
export function VerifiedBadge({ v }: { v: ApiVerification }) {
  if (v.state === "unsigned") return null;

  const verified = v.state === "verified";
  const Icon = verified ? ShieldCheck : ShieldQuestion;
  const detail = verified
    ? `Signed by ${v.signer}.`
    : v.reason ?? "This signature could not be checked.";

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          className={`inline-flex items-center gap-1 border px-1.5 py-0.5 text-micro font-medium uppercase tracking-label ${
            verified
              ? "border-success/40 bg-success/10 text-success"
              : "border-border bg-muted text-muted-foreground"
          }`}
        >
          <Icon className="size-3" />
          {verified ? "Verified" : "Unverified"}
        </button>
      </TooltipTrigger>
      <TooltipContent>{detail}</TooltipContent>
    </Tooltip>
  );
}
