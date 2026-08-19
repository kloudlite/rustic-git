import Link from "next/link";
import { CircleCheck, CircleX, Copy, File, Folder, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { FileTree } from "@/components/repo/file-tree";
import { RefPicker } from "@/components/repo/ref-picker";
import { ScrollArea, ScrollBar } from "@/components/ui/scroll-area";
import { README, REPO, TREE } from "@/lib/mock-repo";

function Markdown({ source }: { source: string }) {
  // The real page renders through a markdown pipeline; for the mock, paragraphs,
  // headings, list items and fenced code are enough to show the shape.
  const blocks = source.trim().split(/\n\n+/);
  return (
    <div className="grid gap-4 text-sm2 leading-relaxed">
      {blocks.map((b, i) => {
        if (b.startsWith("# ")) return <h1 key={i} className="text-title font-semibold tracking-title">{b.slice(2)}</h1>;
        if (b.startsWith("## ")) return <h2 key={i} className="mt-2 text-body font-semibold">{b.slice(3)}</h2>;
        if (b.startsWith("```")) return (
          <ScrollArea key={i} className="border border-border bg-muted/40">
            <pre className="p-3 font-mono text-caption">{b.replace(/```\n?/g, "").trim()}</pre>
            <ScrollBar orientation="horizontal" />
          </ScrollArea>
        );
        if (b.startsWith("- ")) return (
          <ul key={i} className="grid gap-1 pl-5 [list-style:square]">
            {b.split("\n").map((l, j) => <li key={j} dangerouslySetInnerHTML={{ __html: l.slice(2).replace(/`([^`]+)`/g, '<code class="font-mono text-caption bg-muted px-1">$1</code>') }} />)}
          </ul>
        );
        return <p key={i} className="text-foreground/90">{b}</p>;
      })}
    </div>
  );
}

/** Code at the root: tree on the left, listing plus README on the right. The
 *  toolbar carries the ref (moving) and the last commit (a fact about the ref). */
export function CodeView({ owner, dir = "" }: { owner: string; dir?: string }) {
  const base = `/${owner}/${REPO.name}`;
  const entries = TREE[dir] ?? [];
  const crumbs = dir ? dir.split("/") : [];

  return (
    <div className="grid gap-8 lg:grid-cols-code">
      <aside className="hidden lg:block">
        <div className="sticky top-28">
          <FileTree base={base} openDir={dir || undefined} />
        </div>
      </aside>

      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker current={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} />
          {crumbs.length > 0 && (
            <nav aria-label="Path" className="flex items-center gap-1 text-sm2">
              <Link href={base} className="font-medium text-primary underline-offset-4 hover:underline">{REPO.name}</Link>
              {crumbs.map((c, i) => (
                <span key={i} className="flex items-center gap-1">
                  <span className="text-muted-foreground">/</span>
                  {i === crumbs.length - 1
                    ? <span className="font-medium">{c}</span>
                    : <Link href={`${base}/tree/${crumbs.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{c}</Link>}
                </span>
              ))}
            </nav>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Button variant="outline" className="border-edge hover:border-edge-hover"><Copy />Clone</Button>
          </div>
        </div>

        <div className="mt-4 border border-border">
          <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2.5 text-sm2">
            <span className="flex size-6 items-center justify-center bg-muted text-micro font-semibold text-muted-foreground">
              {REPO.head.author.slice(0, 2).toUpperCase()}
            </span>
            <span className="font-medium">{REPO.head.author}</span>
            <span className="min-w-0 flex-1 truncate text-foreground/90">{REPO.head.message}</span>
            {REPO.head.sha && <CircleCheck className="size-4 text-success" aria-label="Pipeline passing" />}
            <Link href={`${base}/commit/${REPO.head.sha}`} className="font-mono text-caption text-primary underline-offset-4 hover:underline">{REPO.head.sha}</Link>
            <span className="text-caption text-muted-foreground">{REPO.head.when}</span>
            <Link href={`${base}/commits`} className="ml-2 inline-flex items-center gap-1 text-caption font-medium text-muted-foreground hover:text-foreground">
              <History className="size-3.5" /> History
            </Link>
          </div>
          <ul className="divide-y divide-border">
            {entries.map((e) => (
              <li key={e.name} className="flex items-center gap-3 px-4 py-2 text-sm2">
                {e.kind === "dir" ? <Folder className="size-4 shrink-0 text-muted-foreground" /> : <File className="size-4 shrink-0 text-muted-foreground" />}
                <Link
                  href={e.kind === "dir" ? `${base}/tree/${dir ? `${dir}/` : ""}${e.name}` : `${base}/blob/${dir ? `${dir}/` : ""}${e.name}`}
                  className="w-44 shrink-0 truncate font-medium underline-offset-4 hover:underline"
                >
                  {e.name}
                </Link>
                <span className="min-w-0 flex-1 truncate text-muted-foreground">{e.message}</span>
                <span className="shrink-0 text-caption text-muted-foreground">{e.when}</span>
              </li>
            ))}
          </ul>
        </div>

        {!dir && (
          <div className="mt-6 border border-border">
            <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-sm2 font-medium">
              <File className="size-4 text-muted-foreground" /> README.md
            </div>
            <div className="max-w-prose px-6 py-6">
              <Markdown source={README} />
            </div>
          </div>
        )}
      </section>
    </div>
  );
}
