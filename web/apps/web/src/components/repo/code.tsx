import Link from "next/link";
import { File, Folder, CornerLeftUp } from "lucide-react";
import type { BundledLanguage } from "shiki";
import { CloneMenu } from "@/components/repo/clone-menu";
import { cloneUrls } from "@/lib/clone";
import { CodeBlock } from "@/components/repo/code-block";
import { RefPicker } from "@/components/repo/ref-picker";
import { FileSearch } from "@/components/repo/file-search";
import { RepoAbout } from "@/components/repo/repo-about";
import { Initials } from "@/components/app/initials";
import { EmptyRepo } from "@/components/repo/empty-repo";
import {
  blob, decodeBlob, defaultBranch, files, lastChanges, log, refs, shortOid, shortRef, tree,
  type Entry,
} from "@/lib/browse";
import { breakdown } from "@/lib/languages";
import type { Contributor } from "@/components/repo/repo-about";
import type { ApiRepo } from "@/lib/api";

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

/** Unix seconds to something a person reads. Coarse on purpose: the exact minute
 *  of a commit three months ago is noise, and the title carries the real date. */
function when(seconds: number) {
  const d = Math.floor((Date.now() / 1000 - seconds));
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} minutes ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} hours ago`;
  if (d < 2592000) return `${Math.floor(d / 86400)} days ago`;
  return new Date(seconds * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

/** Directories first, then files, each alphabetical — the order every git host
 *  uses, and the order the tree is NOT returned in. */
function ordered(entries: Entry[]) {
  return [...entries].sort((a, b) => {
    const kind = Number(b.kind === "tree") - Number(a.kind === "tree");
    return kind !== 0 ? kind : a.name.localeCompare(b.name);
  });
}

function size(bytes: number | null) {
  if (bytes === null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** Code: the ref, the way to take the code away, the listing, and the README.
 *
 *  Everything here comes from the browse API, which is keyed by object id — so a
 *  ref is resolved once and every later call is immutable and cacheable. What the
 *  API does not serve is not drawn: there is no per-file last-commit column,
 *  because that is one log walk per row and nothing serves it. */
export async function CodeView({
  token,
  owner,
  repo,
  meta,
  dir = "",
  refName,
}: {
  token: string;
  owner: string;
  repo: string;
  meta: ApiRepo;
  dir?: string;
  /** From `?ref=`. An unknown one falls back to the default rather than 404s:
   *  a branch can be deleted while someone still holds the link. */
  refName?: string;
}) {
  const base = `/${owner}/${repo}`;
  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);

  const branches = all.value.filter((r) => r.kind === "branch");
  const tags = all.value.filter((r) => r.kind === "tag");
  const fallback = defaultBranch(all.value);
  const head = (refName && all.value.find((r) => shortRef(r.name) === refName)) || fallback;

  // A repo with no refs is not broken — it is new. It has nothing to list, so it
  // gets the one thing it needs: how to put something in it.
  if (!head) return <EmptyRepo owner={owner} repo={repo} urls={cloneUrls(owner, repo)} isPrivate={!meta.public} />;

  const [entries, recent, blobs, touched] = await Promise.all([
    tree(token, owner, repo, head.oid, dir),
    log(token, owner, repo, head.oid, 1),
    // Only at the root: the rail describes the repo, not the directory, so
    // re-fetching the file list on every navigation would buy nothing.
    dir ? Promise.resolve([]) : files(token, owner, repo, head.oid),
    lastChanges(token, owner, repo, head.oid, dir),
  ]);
  if (!entries.ok) throw new Error(entries.message);

  const last = recent.ok ? recent.value[0] : undefined;
  const languages = breakdown(blobs);
  // Go-to-file searches the same walk the language bar came from, so the two
  // agree about what is in the repo and neither costs a second traversal.
  const paths = blobs.map((b: { path: string }) => ({ path: b.path, kind: "file" as const }));
  // Who has been committing, from the log this page already has. Not all of
  // history — the tooltip says "recently" rather than implying a total.
  const byAuthor = new Map<string, number>();
  for (const c of recent.ok ? recent.value : []) {
    byAuthor.set(c.author, (byAuthor.get(c.author) ?? 0) + 1);
  }
  const contributors: Contributor[] = [...byAuthor.entries()]
    .map(([name, commits]) => ({ name, commits }))
    .sort((a, b) => b.commits - a.commits)
    .slice(0, 12);
  const list = ordered(entries.value);
  const q = refName ? `?ref=${encodeURIComponent(refName)}` : "";
  const crumbs = dir ? dir.split("/") : [];
  const parent = (crumbs.length > 1 ? `${base}/tree/${crumbs.slice(0, -1).join("/")}` : base) + q;

  // Only at the root, and only if there is one: a README fetched per directory
  // would be a blob request on every navigation for a file that is usually absent.
  const readmeEntry = dir ? undefined : list.find((e) => e.kind === "blob" && /^readme(\.md)?$/i.test(e.name));
  const readme = readmeEntry
    ? await blob(token, owner, repo, head.oid, readmeEntry.name)
    : undefined;
  const readmeText = readme?.ok ? decodeBlob(readme.value) : undefined;

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker
            current={shortRef(head.name)}
            defaultBranch={fallback ? shortRef(fallback.name) : undefined}
            branches={branches.map((b) => shortRef(b.name))}
            tags={tags.map((t) => shortRef(t.name))}
            base={dir ? `${base}/tree/${dir}` : base}
          />
          {paths.length > 0 && (
            <FileSearch base={base} entries={paths} className="w-full max-w-xs" />
          )}
          <div className="ml-auto">
            <CloneMenu urls={cloneUrls(owner, repo)} />
          </div>
        </div>

        <nav aria-label="Path" className="mt-5 flex min-w-0 items-center gap-1 text-sm2">
          <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">{owner}</Link>
          <span className="text-muted-foreground">/</span>
          <Link href={base + q} className={crumbs.length ? "text-primary underline-offset-4 hover:underline" : "font-medium"}>{repo}</Link>
          {crumbs.map((c, i) => (
            <span key={i} className="flex items-center gap-1">
              <span className="text-muted-foreground">/</span>
              {i === crumbs.length - 1
                ? <span className="font-medium">{c}</span>
                : <Link href={`${base}/tree/${crumbs.slice(0, i + 1).join("/")}${q}`} className="text-primary underline-offset-4 hover:underline">{c}</Link>}
            </span>
          ))}
        </nav>

        <div className="mt-3 border border-border bg-card">
          {last && (
            <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2.5 text-sm2">
              <Initials name={last.author} size={6} />
              <span className="font-medium">{last.author}</span>
              <span className="min-w-0 flex-1 truncate text-foreground/90">{last.message.split("\n")[0]}</span>
              <Link href={`${base}/commit/${last.oid}`} className="font-mono text-caption text-primary underline-offset-4 hover:underline">
                {shortOid(last.oid)}
              </Link>
              <span className="text-caption text-muted-foreground" title={new Date(last.time * 1000).toISOString()}>
                {when(last.time)}
              </span>
            </div>
          )}
          <ul className="divide-y divide-border">
            {dir && (
              <li className="flex items-center gap-4 px-4 py-2 text-sm2">
                <Link href={parent} className="flex items-center gap-2.5 text-muted-foreground transition-colors hover:text-foreground">
                  <CornerLeftUp className="size-4 shrink-0" />
                  <span className="font-mono">..</span>
                </Link>
              </li>
            )}
            {list.map((e) => {
              const path = `${dir ? `${dir}/` : ""}${e.name}`;
              return (
                <li key={e.oid + e.name} className="flex items-center gap-4 px-4 py-2 text-sm2">
                  <div className="flex min-w-0 flex-1 items-center gap-2.5 md:max-w-xs">
                    {e.kind === "tree"
                      ? <Folder className="size-4 shrink-0 text-primary/70" />
                      : <File className="size-4 shrink-0 text-muted-foreground" />}
                    <Link
                      href={`${base}/${e.kind === "tree" ? "tree" : "blob"}/${path}${q}`}
                      className="truncate font-medium underline-offset-4 hover:underline"
                    >
                      {e.name}
                    </Link>
                  </div>
                  {(() => {
                    const c = touched.get(e.name);
                    // Absent when history ran past the server's budget. Nothing is
                    // drawn rather than something wrong.
                    if (!c) return <span className="shrink-0 text-caption text-muted-foreground">{size(e.size)}</span>;
                    return (
                      <>
                        <Link
                          href={`${base}/commit/${c.oid}`}
                          className="hidden min-w-0 flex-1 truncate text-caption text-muted-foreground underline-offset-4 hover:text-foreground hover:underline md:block"
                        >
                          {c.message.split("\n")[0]}
                        </Link>
                        <span
                          className="shrink-0 text-caption text-muted-foreground"
                          title={new Date(c.time * 1000).toISOString()}
                        >
                          {when(c.time)}
                        </span>
                      </>
                    );
                  })()}
                </li>
              );
            })}
            {list.length === 0 && (
              <li className="px-4 py-6 text-center text-sm2 text-muted-foreground">This directory is empty.</li>
            )}
          </ul>
        </div>

        {readmeText && !readmeText.binary && (
          <div className="mt-6 border border-border bg-card">
            <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-sm2 font-medium">
              <File className="size-4 text-muted-foreground" /> {readmeEntry!.name}
            </div>
            <div className="max-w-readme px-8 py-7">
              <Markdown source={readmeText.text} />
            </div>
          </div>
        )}
      </section>

      <aside className="hidden xl:block">
        <RepoAbout
          base={base}
          description={meta.description}
          branches={branches.length}
          tags={tags.length}
          isPrivate={!meta.public}
          languages={languages}
          contributors={contributors}
        />
      </aside>
    </div>
  );
}
