import Link from "next/link";
import { History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/repo/code-block";
import { RefPicker } from "@/components/repo/ref-picker";
import { RepoAbout } from "@/components/repo/repo-about";
import { blob, decodeBlob, defaultBranch, refs, shortRef } from "@/lib/browse";
import type { ApiRepo } from "@/lib/api";

function size(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

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
}: {
  token: string;
  owner: string;
  repo: string;
  meta: ApiRepo;
  path: string;
}) {
  const base = `/${owner}/${repo}`;
  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);
  const head = defaultBranch(all.value);
  if (!head) throw new Error("this repo has no branches");

  const b = await blob(token, owner, repo, head.oid, path);
  if (!b.ok) throw new Error(b.message);
  const decoded = decodeBlob(b.value);
  const bytes = Buffer.from(b.value.bytes_base64, "base64").length;
  const parts = path.split("/");

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker
            current={shortRef(head.name)}
            defaultBranch={shortRef(head.name)}
            branches={all.value.filter((r) => r.kind === "branch").map((r) => shortRef(r.name))}
            tags={all.value.filter((r) => r.kind === "tag").map((r) => shortRef(r.name))}
          />
        </div>

        <nav aria-label="Path" className="mt-5 flex min-w-0 items-center gap-1 text-sm2">
          <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">{owner}</Link>
          <span className="text-muted-foreground">/</span>
          <Link href={base} className="text-primary underline-offset-4 hover:underline">{repo}</Link>
          {parts.map((p, i) => (
            <span key={i} className="flex items-center gap-1">
              <span className="text-muted-foreground">/</span>
              {i === parts.length - 1
                ? <span className="font-medium">{p}</span>
                : <Link href={`${base}/tree/${parts.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{p}</Link>}
            </span>
          ))}
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
              <Button asChild variant="ghost" size="sm" className="text-caption">
                <Link href={`${base}/commits`}><History />History</Link>
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
        <RepoAbout
          base={base}
          description={meta.description}
          branches={all.value.filter((r) => r.kind === "branch").length}
          tags={all.value.filter((r) => r.kind === "tag").length}
          isPrivate={!meta.public}
        />
      </aside>
    </div>
  );
}
