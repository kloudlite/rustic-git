import Link from "next/link";
import { File, Folder, CornerLeftUp } from "lucide-react";
import { CloneMenu } from "@/components/repo/clone-menu";
import { cloneUrls } from "@/lib/clone";
import { CodeBlock } from "@/components/repo/code-block";
import { RefPicker } from "@/components/repo/ref-picker";
import { FileSearch } from "@/components/repo/file-search";
import { RepoAbout } from "@/components/repo/repo-about";
import { Initials } from "@/components/app/initials";
import { EmptyRepo } from "@/components/repo/empty-repo";
import {
  blob, decodeBlob, defaultBranch, lastChanges, refs, resolveRef, shortOid, shortRef, tree,
  type Entry,
} from "@/lib/browse";
import { repoRail } from "@/lib/repo-rail";
import { size, whenSeconds } from "@/lib/time";
import type { ApiRepo } from "@/lib/api";
import { fenceLang } from "@/lib/highlight";
import { pathHref } from "@/lib/utils";

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
          // A bare ``` fence has no word to match, so it now renders as text (was "bash").
          const lang = fenceLang(b.match(/^```(\w+)/)?.[1]);
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

/** Directories first, then files, each alphabetical — the order every git host
 *  uses, and the order the tree is NOT returned in. */
function ordered(entries: Entry[]) {
  return [...entries].sort((a, b) => {
    const kind = Number(b.kind === "tree") - Number(a.kind === "tree");
    return kind !== 0 ? kind : a.name.localeCompare(b.name);
  });
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
  const head = resolveRef(all.value, refName);

  // A repo with no refs is not broken — it is new. It has nothing to list, so it
  // gets the one thing it needs: how to put something in it.
  if (!head) return <EmptyRepo owner={owner} repo={repo} urls={cloneUrls(owner, repo)} isPrivate={!meta.public} />;

  // The rail is a fact about the repo, so it is fetched the same way in every
  // directory — see `repoRail`. Inside a subdirectory these are cache hits: both
  // halves are keyed by the commit id, not by the path.
  const [entries, rail, touched, readmeGuess] = await Promise.all([
    tree(token, owner, repo, head.oid, dir),
    repoRail(token, owner, repo, head.oid),
    lastChanges(token, owner, repo, head.oid, dir),
    // Speculative: most directories that have a README spell it README.md, and
    // fetching it in parallel removes a whole round trip from the repo home.
    // A miss is a cheap 404; any other spelling falls back to the exact fetch.
    blob(token, owner, repo, head.oid, `${dir ? `${dir}/` : ""}README.md`),
  ]);
  if (!entries.ok) throw new Error(entries.message);

  const last = rail.commits[0];
  // Go-to-file searches the same file list the language bar came from, so the
  // two always agree about what is in the repo.
  // ponytail: go-to-file ships at most 5000 paths to the client; server-side
  // search when a repo outgrows that. 10k-file repos were paying a 10k-entry
  // RSC payload on every page.
  const paths = rail.blobs.slice(0, 5000).map((b) => ({ path: b.path, kind: "file" as const }));
  const list = ordered(entries.value);
  const q = refName ? `?ref=${encodeURIComponent(refName)}` : "";
  const crumbs = dir ? dir.split("/") : [];
  const parent = (crumbs.length > 1 ? `${base}/tree/${pathHref(crumbs.slice(0, -1).join("/"))}` : base) + q;

  // A README belongs to the directory it sits in, so it is shown wherever there
  // is one — the same rule everywhere, rather than a file that renders at the
  // root and is silently ignored one level down.
  const readmeEntry = list.find((e) => e.kind === "blob" && /^readme(\.md)?$/i.test(e.name));
  const readme = readmeEntry
    ? readmeEntry.name === "README.md" && readmeGuess.ok
      ? readmeGuess
      : await blob(token, owner, repo, head.oid, `${dir ? `${dir}/` : ""}${readmeEntry.name}`)
    : undefined;
  const readmeText = readme?.ok ? decodeBlob(readme.value) : undefined;

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker
            current={head.kind === "commit" ? shortOid(head.oid) : shortRef(head.name)}
            defaultBranch={fallback ? shortRef(fallback.name) : undefined}
            branches={branches.map((b) => shortRef(b.name))}
            tags={tags.map((t) => shortRef(t.name))}
            base={dir ? `${base}/tree/${pathHref(dir)}` : base}
          />
          {paths.length > 0 && (
            <FileSearch base={base} entries={paths} refName={refName} className="w-full max-w-xs" />
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
                : <Link href={`${base}/tree/${pathHref(crumbs.slice(0, i + 1).join("/"))}${q}`} className="text-primary underline-offset-4 hover:underline">{c}</Link>}
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
                {whenSeconds(last.time)}
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
                      href={`${base}/${e.kind === "tree" ? "tree" : "blob"}/${pathHref(path)}${q}`}
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
                          {whenSeconds(c.time)}
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
          languages={rail.languages}
          contributors={rail.contributors}
        />
      </aside>
    </div>
  );
}
