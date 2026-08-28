import { Terminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { DeleteForm } from "@/components/app/delete-form";
import { revokeCliToken } from "@/app/(shell)/settings/actions";
import { when } from "@/lib/time";
import type { ApiCliToken } from "@/lib/api";

/** The CLI logins this person has approved. There is no "add" here on purpose: a login
 *  starts at the terminal with `kl login` and is approved on /cli/authorize — this list is
 *  only the record of it, and the way to take one back. */
export function CliTokens({ tokens }: { tokens: ApiCliToken[] }) {
  return (
    <>
      <p className="text-sm2 text-muted-foreground">
        Install the CLI with{" "}
        <code className="font-mono text-caption">curl -fsSL https://dev.kloudlite.io/install.sh | sh</code>,
        then run <code className="font-mono text-caption">kl login</code>.
      </p>

      {tokens.length === 0 ? (
        <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          No CLI logins yet.
        </p>
      ) : (
        <ul className="mt-3 divide-y divide-border border border-border bg-card">
          {tokens.map((t) => (
            <li key={t.id} className="flex items-center gap-4 px-4 py-3">
              <Terminal className="size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm2 font-medium">{t.name || "Unnamed device"}</div>
                <div className="mt-0.5 text-caption text-muted-foreground">
                  Signed in {when(Date.parse(t.createdAt))} · expires {when(Date.parse(t.expiresAt))}
                </div>
              </div>
              <DeleteForm
                action={revokeCliToken}
                fields={{ id: t.id }}
                confirm="Revoke this login? That machine will have to run kl login again."
              >
                <Button type="submit" variant="outline" size="sm" className="border-edge text-muted-foreground hover:border-destructive/40 hover:text-destructive">
                  Revoke
                </Button>
              </DeleteForm>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
