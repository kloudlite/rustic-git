import Link from "next/link";
import { ChevronRight, FileCode } from "lucide-react";
import { LARGE_FILE, type DiffLine, type FileDiff, type ParsedDiff } from "@/lib/diff";
import { cn, pathHref } from "@/lib/utils";

/** The files of a diff, however that diff was obtained — one commit, or a whole
 *  branch. Shared so a pull request and a commit render a change identically;
 *  two copies of this drifted apart the last time it was worth doing. */
export function DiffFiles({
  diff,
  base,
  refName,
}: {
  diff: ParsedDiff;
  base: string;
  /** The commit or branch this diff belongs to. Without it every file link opens the
   *  path on the default branch — a file added by the diff 404s there, and a file the
   *  diff changed shows different content than the diff just did. */
  refName?: string;
}) {
  const q = refName ? `?ref=${encodeURIComponent(refName)}` : "";
  return (
    <div className="grid min-w-0 gap-6">
      {diff.files.map((f) => {
          const changed = f.additions + f.deletions;
          // A binary file has nothing to fold and nothing to scroll — it is one
          // line saying so, and folding it would hide that line behind a click.
          const big = !f.binary && changed > LARGE_FILE;
          return (
            // min-w-0 and overflow-hidden together: a grid item's min-width is
            // `auto`, so without them the card cannot shrink below the widest
            // line in the diff and the whole PAGE scrolls sideways instead of
            // the code block doing it.
            <details
              key={f.path}
              id={pathHref(f.path)}
              open={!big}
              className="group min-w-0 scroll-mt-24 overflow-hidden border border-border bg-card"
            >
              <summary className="flex cursor-pointer list-none items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-sm2 [&::-webkit-details-marker]:hidden">
                <ChevronRight className="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-90" />
                <FileCode className="size-4 shrink-0 text-muted-foreground" />
                <Link href={`${base}/blob/${pathHref(f.path)}${q}`} className="truncate font-mono font-medium underline-offset-4 hover:underline">
                  {f.path}
                </Link>
                {f.binary ? (
                  <span className="ml-auto shrink-0 border border-border px-1.5 py-0.5 text-micro font-medium uppercase tracking-label text-muted-foreground">
                    Binary
                  </span>
                ) : (
                  <span className="ml-auto shrink-0 font-mono text-caption">
                    <span className="text-success">+{f.additions}</span>{" "}
                    <span className="text-destructive">−{f.deletions}</span>
                  </span>
                )}
              </summary>
              {big && (
                <p className="border-b border-border px-4 py-2 text-caption text-muted-foreground">
                  {changed} changed lines — folded so the rest of the commit stays readable.
                </p>
              )}
              {f.binary ? (
                <p className="px-4 py-6 text-center text-caption text-muted-foreground">
                  Binary file not shown.
                </p>
              ) : (
                <FileHunks file={f} />
              )}
            </details>
          );
        })}
      {diff.files.length === 0 && (
          <p className="border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
            This commit changed nothing.
          </p>
        )}
    </div>
  );
}

function FileHunks({ file }: { file: FileDiff }) {
  if (file.hunks.length === 0) {
    return <p className="px-4 py-6 text-center text-caption text-muted-foreground">No textual changes.</p>;
  }
  return (
    <div className="w-full overflow-x-auto">
      {/* w-max, not w-full: the table is as wide as its widest line and the scroll
          area moves it, which is what keeps a long line inside this box rather
          than stretching the page. */}
      <table className="w-max min-w-full border-collapse font-mono text-caption leading-5">
        <tbody>
          {file.hunks.map((h, i) => (
            <HunkRows key={i} header={h.header} lines={h.lines} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function HunkRows({ header, lines }: { header: string; lines: DiffLine[] }) {
  return (
    <>
      <tr>
        <td colSpan={4} className="bg-muted/60 px-4 py-1 text-muted-foreground">{header}</td>
      </tr>
      {lines.map((l, i) => (
        <tr
          key={i}
          className={cn(
            l.kind === "add" && "bg-success/10",
            l.kind === "del" && "bg-destructive/10",
          )}
        >
          {/* Two gutters, so a line can be found in the file it came from. Both
              are `user-select: none` — copying a diff should give code, not code
              with a column of numbers welded to the front of every line. */}
          <td className="w-12 select-none px-2 text-right align-top text-muted-foreground/50">{l.old ?? ""}</td>
          <td className="w-12 select-none px-2 text-right align-top text-muted-foreground/50">{l.new ?? ""}</td>
          <td
            aria-hidden
            className={cn(
              "w-6 select-none px-1 text-center align-top",
              l.kind === "add" && "text-success",
              l.kind === "del" && "text-destructive",
              l.kind === "ctx" && "text-muted-foreground/40",
            )}
          >
            {l.kind === "add" ? "+" : l.kind === "del" ? "−" : ""}
          </td>
          <td className="whitespace-pre pr-6">{l.text}</td>
        </tr>
      ))}
    </>
  );
}
