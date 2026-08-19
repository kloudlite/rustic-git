import Link from "next/link";
import { CircleCheck, Copy, File, Folder, GitBranch, History, Scale, Tag } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/repo/code-block";
import { FileTree } from "@/components/repo/file-tree";
import { RefPicker } from "@/components/repo/ref-picker";
import { COMMITS, README, REPO, TREE } from "@/lib/mock-repo";
import type { BundledLanguage } from "shiki";

/** Just enough markdown for a README: headings, paragraphs, lists, inline code,
 *  fenced code through the same highlighter as source files. The real page will
 *  render through a markdown pipeline; the shapes here are the ones it will emit. */
function Markdown({ source }: { source: string }) {
  const blocks = source.trim().split(/\n\n+/);
  const inline = (t: string) =>
    t.split(/(`[^`]+`)/).map((seg, i) =>
      seg.startsWith("`") ? <code key={i} className="bg-muted px-1 font-mono text-caption">{seg.slice(1, -1)}</code> : seg,
    );
  return (
    <div className="grid gap-4 text-sm2 leading-relaxed">
      {blocks.map((b, i) => {
        if (b.startsWith("# ")) return <h1 key={i} className="text-title font-semibold tracking-title">{b.slice(2)}</h1>;
        if (b.startsWith("## ")) return <h2 key={i} className="mt-2 border-b border-border pb-1.5 text-body font-semibold">{b.slice(3)}</h2>;
        if (b.startsWith("```")) {
          const lang = (b.match(/^```(\w+)/)?.[1] ?? "bash") as BundledLanguage;
          const code = b.replace(/^```\w*\n?/, "").replace(/```$/, "").trim();
          return <div key={i} className="border border-border bg-muted/30"><CodeBlock code={code} lang={lang} /></div>;
        }
        if (b.startsWith("- ")) return (
          <ul key={i} className="grid list-square gap-1 pl-5">
            {b.split("\n").map((l, j) => <li key={j}>{inline(l.slice(2))}</li>)}
          </ul>
        );
        return <p key={i} className="text-foreground/90">{inline(b)}</p>;
      })}
    </div>
  );
}

function Stat({ icon: Icon, children, href }: { icon: typeof Tag; children: React.ReactNode; href?: string }) {
  const inner = <><Icon className="size-3.5" />{children}</>;
  const cls = "inline-flex items-center gap-1.5 text-caption text-muted-foreground";
  return href ? <Link href={href} className={`${cls} transition-colors hover:text-foreground`}>{inner}</Link> : <span className={cls}>{inner}</span>;
}

/** Code at the root: what the repo is (a line and four facts), then the tree on the
 *  left and the listing plus README on the right. The toolbar carries the ref (the
 *  moving part) and the last commit (a fact about the ref). */
export function CodeView({ owner, dir = "" }: { owner: string; dir?: string }) {
  const base = `/${owner}/${REPO.name}`;
  const entries = TREE[dir] ?? [];
  const crumbs = dir ? dir.split("/") : [];

  return (
    <div>
      {!dir && (
        <div className="mb-6 flex flex-wrap items-baseline gap-x-6 gap-y-2">
          <p className="max-w-prose text-sm2 text-muted-foreground">{REPO.description}</p>
          <div className="ml-auto flex flex-wrap items-center gap-x-5 gap-y-1">
            <Stat icon={History} href={`${base}/commits`}>{COMMITS.length} commits</Stat>
            <Stat icon={GitBranch}>{REPO.branches.length} branches</Stat>
            <Stat icon={Tag}>{REPO.tags.length} tags</Stat>
            <Stat icon={Scale}>SSPL-1.0</Stat>
          </div>
        </div>
      )}

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
              <CircleCheck className="size-4 text-success" aria-label="Pipeline passing" />
              <Link href={`${base}/commit/${REPO.head.sha}`} className="font-mono text-caption text-primary underline-offset-4 hover:underline">{REPO.head.sha}</Link>
              <span className="text-caption text-muted-foreground">{REPO.head.when}</span>
            </div>
            <ul className="divide-y divide-border">
              {entries.map((e) => (
                <li key={e.name} className="grid grid-cols-listing items-center gap-4 px-4 py-2 text-sm2">
                  <div className="flex min-w-0 items-center gap-2.5">
                    {e.kind === "dir" ? <Folder className="size-4 shrink-0 text-primary/70" /> : <File className="size-4 shrink-0 text-muted-foreground" />}
                    <Link
                      href={e.kind === "dir" ? `${base}/tree/${dir ? `${dir}/` : ""}${e.name}` : `${base}/blob/${dir ? `${dir}/` : ""}${e.name}`}
                      className="truncate font-medium underline-offset-4 hover:underline"
                    >
                      {e.name}
                    </Link>
                  </div>
                  <span className="min-w-0 truncate text-muted-foreground">{e.message}</span>
                  <span className="text-right text-caption text-muted-foreground">{e.when}</span>
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
    </div>
  );
}
