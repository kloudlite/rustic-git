import Link from "next/link";
import { notFound } from "next/navigation";
import { BackLink } from "@/components/repo/back-link";
import { CopyButton } from "@/components/repo/copy-button";
import { Initials } from "@/components/app/initials";
import { commit as fetchCommit, shortOid } from "@/lib/browse";
import { verifyCommit } from "@/lib/api";
import { VerifiedBadge } from "@/components/repo/verified-badge";
import { parseDiff } from "@/lib/diff";
import { DiffFiles } from "@/components/repo/diff-files";
import { commitBody, commitTitle } from "@/components/repo/commit-meta";
import { whenSeconds } from "@/lib/time";

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
  const [r, verification] = await Promise.all([
    fetchCommit(token, owner, repo, sha),
    // A signature that cannot be checked is reported as unsigned rather than as
    // an error: the commit is still worth reading.
    verifyCommit(token, owner, repo, sha),
  ]);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
  const c = r.value;
  const diff = parseDiff(c.diff);
  const body = commitBody(c.message);

  return (
    <section className="min-w-0">
      <BackLink href={`${base}/commits`}>Commits</BackLink>

      <div className="mt-3 border border-border bg-card">
        <div className="px-5 py-4">
          <div className="flex flex-wrap items-center gap-2.5">
            <h1 className="text-body font-semibold leading-snug">{commitTitle(c.message)}</h1>
            {verification.ok && <VerifiedBadge v={verification.value} />}
          </div>
          {body && (
            <p className="mt-2 max-w-prose whitespace-pre-line text-sm2 leading-relaxed text-muted-foreground">{body}</p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-border bg-muted/40 px-5 py-2.5 text-caption text-muted-foreground">
          <span className="flex items-center gap-2">
            <Initials name={c.author} size={6} />
            <span className="font-medium text-foreground/80">{c.author}</span> committed{" "}
            <span title={new Date(c.time * 1000).toISOString()}>{whenSeconds(c.time)}</span>
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

      <div className="mt-3">
        <DiffFiles diff={diff} base={base} />
      </div>

    </section>
  );
}
