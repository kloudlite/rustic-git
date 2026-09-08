import Link from "next/link";
import { GitBranch, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { DeleteForm } from "@/components/app/delete-form";
import { listProtection, listPulls } from "@/lib/api";
import { refs, shortOid } from "@/lib/browse";
import { branchName, DEFAULT_BRANCH, orderBranches, protectedBy } from "@/lib/branches";
import { deleteBranch } from "@/app/(shell)/[owner]/[repo]/branches/actions";

function Pill({ children }: { children: React.ReactNode }) {
  return (
    <span className="shrink-0 border border-border px-1.5 py-0.5 text-micro font-medium text-muted-foreground">
      {children}
    </span>
  );
}

/** Every branch, with what stops it being deleted stated on the row rather than only
 *  discovered by clicking. The three reasons are exactly the three the api refuses on,
 *  so a disabled button here is a refusal the fleet would have made anyway — the server
 *  is still the gate, this is only the explanation. */
export async function BranchesView({ token, owner, repo }: { token: string; owner: string; repo: string }) {
  const base = `/${owner}/${repo}`;
  const [all, rules, pulls] = await Promise.all([
    refs(token, owner, repo),
    listProtection(token, owner, repo),
    listPulls(token, owner, repo),
  ]);
  if (!all.ok) throw new Error(all.message);
  // Protection and pulls only add marks to a row; a repo whose rules cannot be read
  // still lists its branches, and the server refuses the delete regardless.
  const protection = rules.ok ? rules.value : [];
  const openPull = new Map(
    (pulls.ok ? pulls.value : []).filter((p) => p.state === "open").map((p) => [p.head, p.number]),
  );

  const branches = orderBranches(all.value);

  return (
    <section className="min-w-0">
      <h1 className="text-title font-semibold tracking-title">Branches</h1>

      {branches.length === 0 ? (
        <div className="mt-6 border border-border bg-card px-5 py-14 text-center">
          <p className="text-sm2 font-medium">No branches</p>
          <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
            Push one and it shows up here.
          </p>
        </div>
      ) : (
        <ul className="mt-6 divide-y divide-border border border-border bg-card">
          {branches.map((b) => {
            const name = branchName(b);
            const rule = protectedBy(protection, name);
            const pull = openPull.get(name);
            const blocked =
              name === DEFAULT_BRANCH ? "The default branch cannot be deleted."
              : rule ? `Protected by ${rule}.`
              : pull ? `Close or merge pull request #${pull} first.`
              : undefined;
            return (
              <li key={b.name} className="flex items-center gap-4 px-5 py-3.5">
                <GitBranch className="size-4 shrink-0 text-muted-foreground" />
                <Link
                  href={`${base}?ref=${encodeURIComponent(name)}`}
                  className="min-w-0 truncate text-sm2 font-medium underline-offset-4 hover:underline"
                >
                  {name}
                </Link>
                <Link
                  href={`${base}/commit/${b.oid}`}
                  className="shrink-0 font-mono text-caption text-primary underline-offset-4 hover:underline"
                >
                  {shortOid(b.oid)}
                </Link>
                <span className="flex flex-1 flex-wrap items-center gap-1.5">
                  {name === DEFAULT_BRANCH && <Pill>default</Pill>}
                  {rule && <Pill>protected</Pill>}
                  {pull !== undefined && (
                    <Link href={`${base}/pulls/${pull}`} className="shrink-0">
                      <Pill>PR #{pull}</Pill>
                    </Link>
                  )}
                </span>
                {blocked ? (
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled
                    title={blocked}
                    aria-label={`Cannot delete ${name}: ${blocked}`}
                    className="text-muted-foreground"
                  >
                    <Trash2 />
                  </Button>
                ) : (
                  <DeleteForm
                    action={deleteBranch}
                    fields={{ owner, repo, branch: name, oid: b.oid }}
                    confirm={`Delete branch ${name}? The commits stay reachable only while something else points at them.`}
                  >
                    <Button
                      type="submit"
                      variant="ghost"
                      size="sm"
                      className="text-muted-foreground hover:text-destructive"
                      aria-label={`Delete ${name}`}
                    >
                      <Trash2 />
                    </Button>
                  </DeleteForm>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
