import Link from "next/link";
import { CircleCheck, CircleX, Copy } from "lucide-react";
import { RefPicker } from "@/components/repo/ref-picker";
import { COMMITS, REPO } from "@/lib/mock-repo";

const DAYS = ["Today", "Yesterday", "Earlier"];

/** History of a ref. No file tree: a commit belongs to no branch, and the tree is a
 *  fact about a ref, not about a list of commits. */
export function CommitsView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <section>
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker current={REPO.defaultBranch} defaultBranch={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} />
        <span className="text-sm2 text-muted-foreground">{COMMITS.length} commits</span>
      </div>

      <div className="mt-6 grid gap-8">
        {DAYS.map((day) => {
          const list = COMMITS.filter((c) => c.day === day);
          if (!list.length) return null;
          return (
            <div key={day}>
              <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">{day}</h2>
              <ul className="mt-3 divide-y divide-border border border-border">
                {list.map((c) => (
                  <li key={c.sha} className="flex items-center gap-4 px-5 py-3.5">
                    <div className="min-w-0 flex-1">
                      <Link href={`${base}/commit/${c.sha}`} className="block truncate text-sm2 font-medium underline-offset-4 hover:underline">
                        {c.message}
                      </Link>
                      <p className="mt-1 text-caption text-muted-foreground">
                        <span className="font-medium text-foreground/80">{c.author}</span> committed {c.when}
                      </p>
                    </div>
                    {c.ok === false
                      ? <CircleX className="size-4 shrink-0 text-destructive" aria-label="Pipeline failed" />
                      : <CircleCheck className="size-4 shrink-0 text-success" aria-label="Pipeline passed" />}
                    <div className="flex shrink-0 items-center border border-edge">
                      <Link href={`${base}/commit/${c.sha}`} className="px-2.5 py-1 font-mono text-caption text-primary hover:bg-muted">{c.sha}</Link>
                      <button type="button" aria-label="Copy sha" className="border-l border-edge px-2 py-1 text-muted-foreground hover:bg-muted hover:text-foreground">
                        <Copy className="size-3.5" />
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            </div>
          );
        })}
      </div>
    </section>
  );
}
