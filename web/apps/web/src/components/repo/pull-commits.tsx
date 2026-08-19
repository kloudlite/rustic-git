import Link from "next/link";
import { CircleCheck, Copy } from "lucide-react";
import { PULL, REPO } from "@/lib/mock-repo";

/** The PR's commits, oldest first — the order they will land in. */
export function PullCommits({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <ul className="mt-6 divide-y divide-border border border-border">
      {[...PULL.commits].reverse().map((c) => (
        <li key={c.sha} className="flex items-center gap-4 px-5 py-3.5">
          <div className="min-w-0 flex-1">
            <Link href={`${base}/commit/${c.sha}`} className="block truncate text-sm2 font-medium underline-offset-4 hover:underline">{c.message}</Link>
            <p className="mt-1 text-caption text-muted-foreground"><span className="font-medium text-foreground/80">{c.author}</span> committed {c.when}</p>
          </div>
          <CircleCheck className="size-4 shrink-0 text-success" aria-label="Checks passed" />
          <div className="flex shrink-0 items-center border border-edge">
            <Link href={`${base}/commit/${c.sha}`} className="px-2.5 py-1 font-mono text-caption text-primary hover:bg-muted">{c.sha}</Link>
            <button type="button" aria-label="Copy sha" className="border-l border-edge px-2 py-1 text-muted-foreground hover:bg-muted hover:text-foreground"><Copy className="size-3.5" /></button>
          </div>
        </li>
      ))}
    </ul>
  );
}
