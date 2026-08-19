import { CircleCheck, FileCode } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { DIFF, PULL } from "@/lib/mock-repo";
import { cn } from "@/lib/utils";

/** Files changed: a jump list of files on the left, every diff on the right, and
 *  the review verdict at the end where a reviewer arrives after reading. */
export function PullFiles() {
  return (
    <div className="mt-6 grid gap-8 lg:grid-cols-code">
      <aside className="hidden lg:block">
        <div className="sticky top-28">
          <p className="text-caption text-muted-foreground">
            {PULL.stats.files} files · <span className="text-success">+{PULL.stats.additions}</span> <span className="text-destructive">−{PULL.stats.deletions}</span>
          </p>
          <ul className="mt-2 grid gap-px text-sm2">
            {DIFF.files.map((f) => (
              <li key={f.path}>
                <a href={`#${f.path}`} className="flex h-7 items-center gap-2 px-2 hover:bg-muted">
                  <FileCode className="size-4 shrink-0 text-muted-foreground" />
                  <span className="truncate font-mono text-caption">{f.path}</span>
                </a>
              </li>
            ))}
          </ul>
        </div>
      </aside>

      <section className="grid min-w-0 gap-6">
        {DIFF.files.map((f) => (
          <div key={f.path} id={f.path} className="scroll-mt-28 border border-border">
            <div className="flex items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-sm2">
              <FileCode className="size-4 text-muted-foreground" />
              <span className="font-mono font-medium">{f.path}</span>
              <span className="ml-auto font-mono text-caption"><span className="text-success">+{f.additions}</span> <span className="text-destructive">−{f.deletions}</span></span>
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

        <div className="border border-border">
          <div className="border-b border-border bg-muted/40 px-4 py-2.5 text-sm2 font-medium">Finish your review</div>
          <div className="p-4">
            <textarea rows={3} placeholder="Leave a summary comment" className="block w-full resize-y border border-input bg-transparent px-3 py-2 text-sm2 outline-none placeholder:text-muted-foreground focus-visible:border-ring" />
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button className="bg-success text-primary-foreground hover:bg-success/90"><CircleCheck />Approve</Button>
              <Button variant="outline" className="border-edge hover:border-edge-hover">Request changes</Button>
              <Button variant="outline" className="border-edge hover:border-edge-hover">Comment</Button>
            </div>
          </div>
        </div>
      </section>
    </div>
  );
}

function HunkRows({ header, lines }: { header: string; lines: [string, string][] }) {
  return (
    <>
      <tr><td colSpan={2} className="bg-muted/60 px-4 py-1 text-muted-foreground">{header}</td></tr>
      {lines.map(([sign, text], i) => (
        <tr key={i} className={cn(sign === "+" && "bg-success/10", sign === "-" && "bg-destructive/10")}>
          <td className={cn("w-8 select-none pl-3 pr-2 text-center", sign === "+" ? "text-success" : sign === "-" ? "text-destructive" : "text-muted-foreground/40")}>{sign.trim() || " "}</td>
          <td className="whitespace-pre pr-6 text-foreground/90">{text}</td>
        </tr>
      ))}
    </>
  );
}
