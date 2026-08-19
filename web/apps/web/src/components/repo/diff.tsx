import Link from "next/link";
import { FileCode } from "lucide-react";
import { ScrollArea } from "@/components/ui/scroll-area";
import { BackLink } from "@/components/repo/back-link";
import { DIFF, REPO } from "@/lib/mock-repo";
import { cn } from "@/lib/utils";

/** One commit. Header states the commit; below it, every file's hunks. Green and
 *  red carry a +/- in the gutter as well, so colour is never the only signal. */
export function DiffView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <section>
      <BackLink href={`${base}/commits`}>Commits</BackLink>
      <div className="mt-3 border border-border">
        <div className="px-5 py-4">
          <h1 className="text-body font-semibold leading-snug">{DIFF.message}</h1>
          <p className="mt-2 max-w-prose whitespace-pre-line text-sm2 leading-relaxed text-muted-foreground">{DIFF.body}</p>
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-border bg-muted/40 px-5 py-2.5 text-caption text-muted-foreground">
          <span><span className="font-medium text-foreground/80">{DIFF.author}</span> committed {DIFF.when}</span>
          <span className="ml-auto flex items-center gap-4">
            <span>commit <span className="font-mono text-foreground">{DIFF.sha}</span></span>
            <span>parent <Link href={`${base}/commit/${DIFF.parents[0]}`} className="font-mono text-primary underline-offset-4 hover:underline">{DIFF.parents[0]}</Link></span>
          </span>
        </div>
      </div>

      <p className="mt-6 text-sm2 text-muted-foreground">
        {DIFF.stats.files} files changed ·{" "}
        <span className="font-medium text-success">+{DIFF.stats.additions}</span>{" "}
        <span className="font-medium text-destructive">−{DIFF.stats.deletions}</span>
      </p>

      <div className="mt-3 grid gap-6">
        {DIFF.files.map((f) => (
          <div key={f.path} className="border border-border">
            <div className="flex items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-sm2">
              <FileCode className="size-4 text-muted-foreground" />
              <span className="font-mono font-medium">{f.path}</span>
              <span className="ml-auto font-mono text-caption">
                <span className="text-success">+{f.additions}</span>{" "}
                <span className="text-destructive">−{f.deletions}</span>
              </span>
            </div>
            <ScrollArea orientation="horizontal" className="w-full">
              <table className="w-max min-w-full border-collapse font-mono text-caption leading-5">
                <tbody>
                  {f.hunks.map((h, hi) => (
                    <HunkRows key={hi} header={h.header} lines={h.lines as [string, string][]} />
                  ))}
                </tbody>
              </table>
            </ScrollArea>
          </div>
        ))}
      </div>
    </section>
  );
}

function HunkRows({ header, lines }: { header: string; lines: [string, string][] }) {
  return (
    <>
      <tr>
        <td colSpan={2} className="bg-muted/60 px-4 py-1 text-muted-foreground">{header}</td>
      </tr>
      {lines.map(([sign, text], i) => (
        <tr
          key={i}
          className={cn(
            sign === "+" && "bg-success/10",
            sign === "-" && "bg-destructive/10",
          )}
        >
          <td className={cn(
            "w-8 select-none pl-3 pr-2 text-center",
            sign === "+" ? "text-success" : sign === "-" ? "text-destructive" : "text-muted-foreground/40",
          )}>
            {sign.trim() || " "}
          </td>
          <td className="whitespace-pre pr-6 text-foreground/90">{text}</td>
        </tr>
      ))}
    </>
  );
}
