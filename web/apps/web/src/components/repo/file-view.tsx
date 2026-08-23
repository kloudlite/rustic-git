import Link from "next/link";
import { Suspense } from "react";
import { notFound } from "next/navigation";
import { History, Pencil } from "lucide-react";
import { CopyButton } from "@/components/repo/copy-button";
import { FileActions } from "@/components/repo/file-actions";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/repo/code-block";
import { RefPicker } from "@/components/repo/ref-picker";
import { RepoAbout } from "@/components/repo/repo-about";
import { blob, decodeBlob, defaultBranch, refs, resolveRef, shortOid, shortRef, type Ref } from "@/lib/browse";
import { repoRail } from "@/lib/repo-rail";
import { size } from "@/lib/time";
import type { ApiRepo } from "@/lib/api";
import { pathHref } from "@/lib/utils";

/** A file: the path that got here, and the bytes. A blob is arbitrary binary, so
 *  the two cases the API distinguishes are both drawn — binary is described, never
 *  rendered, and a blob past the API's inline cap says so rather than pretending
 *  the file ends there. */
export async function FileView({
  token,
  owner,
  repo,
  meta,
  path,
  refName,
}: {
  token: string;
  owner: string;
  repo: string;
  meta: ApiRepo;
  path: string;
  refName?: string;
}) {
  const base = `/${owner}/${repo}`;
  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);
  const fallback = defaultBranch(all.value);
  const head = resolveRef(all.value, refName);
  if (!head) throw new Error("this repo has no branches");
  const q = refName ? `?ref=${encodeURIComponent(refName)}` : "";

  const b = await blob(token, owner, repo, head.oid, path);
  if (!b.ok) {
    // A path that is not in this tree is a 404, same as a repo that is not here.
    if (b.kind === "notFound") notFound();
    throw new Error(b.message);
  }
  const decoded = decodeBlob(b.value);
  const bytes = Buffer.from(b.value.bytes_base64, "base64").length;
  const parts = path.split("/");

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker
            current={head.kind === "commit" ? shortOid(head.oid) : shortRef(head.name)}
            defaultBranch={fallback ? shortRef(fallback.name) : undefined}
            branches={all.value.filter((r) => r.kind === "branch").map((r) => shortRef(r.name))}
            tags={all.value.filter((r) => r.kind === "tag").map((r) => shortRef(r.name))}
            base={base}
          />
        </div>

        <nav aria-label="Path" className="mt-5 flex min-w-0 items-center gap-1 text-sm2">
          <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">{owner}</Link>
          <span className="text-muted-foreground">/</span>
          <Link href={base + q} className="text-primary underline-offset-4 hover:underline">{repo}</Link>
          {parts.map((p, i) => (
            <span key={i} className="flex items-center gap-1">
              <span className="text-muted-foreground">/</span>
              {i === parts.length - 1
                ? <span className="font-medium">{p}</span>
                : <Link href={`${base}/tree/${pathHref(parts.slice(0, i + 1).join("/"))}${q}`} className="text-primary underline-offset-4 hover:underline">{p}</Link>}
            </span>
          ))}
          <CopyButton value={path} label="Copy the path" className="ml-1" />
        </nav>

        <div className="mt-3 border border-border bg-card">
          <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2 text-caption text-muted-foreground">
            {!decoded.binary && <><span>{decoded.text.split("\n").length} lines</span><span aria-hidden>·</span></>}
            <span>{size(bytes)}</span>
            {b.value.truncated && (
              <>
                <span aria-hidden>·</span>
                <span className="font-medium text-foreground">truncated</span>
              </>
            )}
            <div className="ml-auto flex items-center gap-1">
              {!decoded.binary && <FileActions text={decoded.text} filename={parts.at(-1) ?? "file"} />}
              {/* Only what can actually be edited: a textarea would turn binary
                  into mojibake and commit that, a truncated blob would commit the
                  part that was served and silently drop the rest, and a tag has
                  no branch for the commit to land on. */}
              {!decoded.binary && !b.value.truncated && head.kind === "branch" && (
                <Button asChild variant="ghost" size="sm" className="text-caption">
                  <Link href={`${base}/edit/${pathHref(path)}?ref=${encodeURIComponent(shortRef(head.name))}`}>
                    <Pencil />Edit
                  </Link>
                </Button>
              )}
              <Button asChild variant="ghost" size="sm" className="text-caption">
                <Link href={`${base}/commits${q}`}><History />History</Link>
              </Button>
            </div>
          </div>
          {decoded.binary ? (
            <p className="px-4 py-10 text-center text-sm2 text-muted-foreground">
              This is a binary file and is not shown.
            </p>
          ) : (
            <CodeBlock code={decoded.text} path={path} />
          )}
        </div>

        {b.value.truncated && (
          <p className="mt-3 text-caption text-muted-foreground">
            Only the first part of this file is shown. Clone the repo to read all of it.
          </p>
        )}
      </section>

      <aside className="hidden xl:block">
        {/* The rail is a walk of the whole tree plus 50 commits — the file must
            not wait for it. Suspense streams it in after the bytes are on screen. */}
        <Suspense fallback={null}>
          <FileRail token={token} owner={owner} repo={repo} meta={meta} base={base} all={all.value} oid={head.oid} />
        </Suspense>
      </aside>
    </div>
  );
}

async function FileRail({
  token,
  owner,
  repo,
  meta,
  base,
  all,
  oid,
}: {
  token: string;
  owner: string;
  repo: string;
  meta: ApiRepo;
  base: string;
  all: Ref[];
  oid: string;
}) {
  const rail = await repoRail(token, owner, repo, oid);
  return (
    <RepoAbout
      base={base}
      description={meta.description}
      branches={all.filter((r) => r.kind === "branch").length}
      tags={all.filter((r) => r.kind === "tag").length}
      isPrivate={!meta.public}
      languages={rail.languages}
      contributors={rail.contributors}
    />
  );
}
