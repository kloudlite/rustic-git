import Link from "next/link";
import { CircleCheck, CornerLeftUp, File, Folder } from "lucide-react";
import { CloneMenu } from "@/components/repo/clone-menu";
import { CodeBlock } from "@/components/repo/code-block";
import { FileSearch } from "@/components/repo/file-search";
import { RefPicker } from "@/components/repo/ref-picker";
import { RepoAbout } from "@/components/repo/repo-about";
import { PATHS, README, REPO, TREE } from "@/lib/mock-repo";
import type { BundledLanguage } from "shiki";
import { Initials } from "@/components/app/initials";

/** Just enough markdown for a README: headings, paragraphs, lists, inline code,
 *  fenced code through the same highlighter as source files. */
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

/** Code: one listing that browses, one box that jumps. The toolbar carries the ref
 *  (the moving part), Go to file, and the ways to take the code away; the listing's
 *  header is the last commit that touched this path; inside a directory the first
 *  row is the way up. What the repo *is* sits in the rail. */
export function CodeView({ owner, dir = "" }: { owner: string; dir?: string }) {
  const base = `/${owner}/${REPO.name}`;
  const entries = TREE[dir] ?? [];
  const crumbs = dir ? dir.split("/") : [];
  const parent = crumbs.length > 1 ? `${base}/tree/${crumbs.slice(0, -1).join("/")}` : base;

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker current={REPO.defaultBranch} defaultBranch={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} />
          <FileSearch base={base} entries={PATHS} className="w-full max-w-xs" />
          <div className="ml-auto">
            <CloneMenu owner={owner} repo={REPO.name} />
          </div>
        </div>

        <nav aria-label="Path" className="mt-5 flex min-w-0 items-center gap-1 text-sm2">
          <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">{owner}</Link>
          <span className="text-muted-foreground">/</span>
          <Link href={base} className={crumbs.length ? "text-primary underline-offset-4 hover:underline" : "font-medium"}>{REPO.name}</Link>
          {crumbs.map((c, i) => (
            <span key={i} className="flex items-center gap-1">
              <span className="text-muted-foreground">/</span>
              {i === crumbs.length - 1
                ? <span className="font-medium">{c}</span>
                : <Link href={`${base}/tree/${crumbs.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{c}</Link>}
            </span>
          ))}
        </nav>

        <div className="mt-3 border border-border bg-card">
          <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2.5 text-sm2">
            <Initials name={REPO.head.author} size={6} />
            <span className="font-medium">{REPO.head.author}</span>
            <span className="min-w-0 flex-1 truncate text-foreground/90">{REPO.head.message}</span>
            <CircleCheck className="size-4 text-success" aria-label="Pipeline passing" />
            <Link href={`${base}/commit/${REPO.head.sha}`} className="font-mono text-caption text-primary underline-offset-4 hover:underline">{REPO.head.sha}</Link>
            <span className="text-caption text-muted-foreground">{REPO.head.when}</span>
          </div>
          <ul className="divide-y divide-border">
            {dir && (
              <li className="grid grid-cols-listing items-center gap-4 px-4 py-2 text-sm2">
                <Link href={parent} className="flex items-center gap-2.5 text-muted-foreground transition-colors hover:text-foreground">
                  <CornerLeftUp className="size-4 shrink-0" />
                  <span className="font-mono">..</span>
                </Link>
                <span />
                <span />
              </li>
            )}
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
          <div className="mt-6 border border-border bg-card">
            <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-sm2 font-medium">
              <File className="size-4 text-muted-foreground" /> README.md
            </div>
            <div className="max-w-readme px-8 py-7">
              <Markdown source={README} />
            </div>
          </div>
        )}
      </section>

      <aside className="hidden xl:block">
        <RepoAbout base={base} />
      </aside>
    </div>
  );
}
