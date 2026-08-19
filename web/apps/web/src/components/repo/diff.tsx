import Link from "next/link";
import { ChevronRight, FileCode } from "lucide-react";
import { ScrollArea, ScrollBar } from "@/components/ui/scroll-area";
import { BackLink } from "@/components/repo/back-link";
import { CopyButton } from "@/components/repo/copy-button";
import { Initials } from "@/components/app/initials";
import { commit as fetchCommit, shortOid } from "@/lib/browse";
import { parseDiff, LARGE_FILE, type DiffLine, type FileDiff } from "@/lib/diff";
import { commitBody, commitTitle, when } from "@/components/repo/commit-meta";
import { cn } from "@/lib/utils";

/** One commit: what it says, then every file it touched.
 *
 *  Added and removed lines carry a `+`/`−` in the gutter as well as a colour, so
 *  the diff is readable without relying on colour alone. */
export async function DiffView({
  token,
  owner,
  repo,
  sha,
}: {
  token: string;
  owner: string;
  repo: string;
  sha: string;
}) {
  const base = `/${owner}/${repo}`;
  const r = await fetchCommit(token, owner, repo, sha);
  if (!r.ok) throw new Error(r.message);
  const c = r.value;
  const diff = parseDiff(c.diff);
  const body = commitBody(c.message);

  return (
    <section className="min-w-0">
      <BackLink href={`${base}/commits`}>Commits</BackLink>

      <div className="mt-3 border border-border bg-card">
        <div className="px-5 py-4">
          <h1 className="text-body font-semibold leading-snug">{commitTitle(c.message)}</h1>
          {body && (
            <p className="mt-2 max-w-prose whitespace-pre-line text-sm2 leading-relaxed text-muted-foreground">{body}</p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-border bg-muted/40 px-5 py-2.5 text-caption text-muted-foreground">
          <span className="flex items-center gap-2">
            <Initials name={c.author} size={6} />
            <span className="font-medium text-foreground/80">{c.author}</span> committed{" "}
            <span title={new Date(c.time * 1000).toISOString()}>{when(c.time)}</span>
          </span>
          <span className="ml-auto flex items-center gap-4">
            <span className="flex items-center gap-1">
              commit <span className="font-mono text-foreground">{shortOid(c.oid)}</span>
              <CopyButton value={c.oid} label="Copy the full sha" />
            </span>
            {c.parents[0] && (
              <span>
                parent{" "}
                <Link href={`${base}/commit/${c.parents[0]}`} className="font-mono text-primary underline-offset-4 hover:underline">
                  {shortOid(c.parents[0])}
                </Link>
              </span>
            )}
          </span>
        </div>
      </div>

      <p className="mt-6 text-sm2 text-muted-foreground">
        {diff.files.length === 1 ? "1 file changed" : `${diff.files.length} files changed`} ·{" "}
        <span className="font-medium text-success">+{diff.additions}</span>{" "}
        <span className="font-medium text-destructive">−{diff.deletions}</span>
      </p>

      {diff.truncated && (
        <p className="mt-3 border-l-2 border-warning bg-warning/5 py-2 pl-4 text-caption text-muted-foreground">
          This commit is too large to show in full. The files below are only part of
          it — clone the repo to read the rest.
        </p>
      )}

      <div className="mt-3 grid min-w-0 gap-6">
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
              open={!big}
              className="group min-w-0 overflow-hidden border border-border bg-card"
            >
              <summary className="flex cursor-pointer list-none items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-sm2 [&::-webkit-details-marker]:hidden">
                <ChevronRight className="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-90" />
                <FileCode className="size-4 shrink-0 text-muted-foreground" />
                <span className="truncate font-mono font-medium">{f.path}</span>
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
    </section>
  );
}

function FileHunks({ file }: { file: FileDiff }) {
  if (file.hunks.length === 0) {
    return <p className="px-4 py-6 text-center text-caption text-muted-foreground">No textual changes.</p>;
  }
  return (
    <ScrollArea className="w-full">
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
      <ScrollBar orientation="horizontal" />
    </ScrollArea>
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
