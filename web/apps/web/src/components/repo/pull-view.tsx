import Link from "next/link";
import { GitCommitHorizontal, GitMerge } from "lucide-react";
import { BackLink } from "@/components/repo/back-link";
import { Initials } from "@/components/app/initials";
import { StateBadge } from "@/components/repo/pull-state";
import { PullActions, CommentBox } from "@/components/repo/pull-actions";
import { DiffFiles } from "@/components/repo/diff-files";
import { compareBranches, getPull } from "@/lib/api";
import { parseDiff } from "@/lib/diff";
import { commitTitle } from "@/components/repo/commit-meta";
import { shortOid } from "@/lib/browse";
import { whenSeconds } from "@/lib/time";

/**
 * One proposed change.
 *
 * The description and the conversation come from the directory; the commits and
 * the diff are read from git RIGHT NOW, against the two branches the change names.
 * That is why a push updates a PR without anything having to write to it — and
 * why a merged PR still shows what it contained.
 */
export async function PullView({
  token,
  owner,
  repo,
  number,
}: {
  token: string;
  owner: string;
  repo: string;
  number: number;
}) {
  const base = `/${owner}/${repo}`;
  const pull = await getPull(token, owner, repo, number);
  if (!pull.ok) throw new Error(pull.message);
  const pr = pull.value;

  const cmp = await compareBranches(token, owner, repo, pr.base, pr.head);
  const comparison = cmp.ok ? cmp.value : null;
  const diff = comparison ? parseDiff(comparison.diff) : null;

  return (
    <section className="min-w-0">
      <BackLink href={`${base}/pulls`}>Pull requests</BackLink>

      <header className="mt-3">
        <h1 className="text-title font-semibold leading-snug tracking-title">
          {pr.title} <span className="font-normal text-muted-foreground">#{pr.number}</span>
        </h1>
        <div className="mt-3 flex flex-wrap items-center gap-3 text-sm2 text-muted-foreground">
          <StateBadge state={pr.state} />
          <span>
            <span className="font-medium text-foreground/80">{pr.author}</span> wants to merge{" "}
            {comparison && (
              <>
                {comparison.commits.length}{" "}
                {comparison.commits.length === 1 ? "commit" : "commits"}{" "}
              </>
            )}
            into <code className="font-mono text-caption text-foreground">{pr.base}</code> from{" "}
            <code className="font-mono text-caption text-foreground">{pr.head}</code>
          </span>
        </div>
      </header>

      {pr.body && (
        <p className="mt-5 max-w-prose whitespace-pre-line border-l-2 border-border pl-4 text-sm2 leading-relaxed text-foreground/90">
          {pr.body}
        </p>
      )}

      <PullActions
        owner={owner}
        repo={repo}
        number={pr.number}
        state={pr.state}
        baseBranch={pr.base}
        mergeability={pr.mergeability}
        job={pr.merge}
      />

      {pr.comments.length > 0 && (
        <ul className="mt-6 grid gap-3">
          {pr.comments.map((c, i) => (
            <li key={i} className="border border-border bg-card">
              <div className="flex items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-caption">
                <Initials name={c.author} size={6} />
                <span className="font-medium text-foreground/80">{c.author}</span>
              </div>
              <p className="whitespace-pre-line px-4 py-3 text-sm2 leading-relaxed">{c.body}</p>
            </li>
          ))}
        </ul>
      )}

      <CommentBox owner={owner} repo={repo} number={pr.number} />

      <h2 className="mt-10 text-caption font-semibold uppercase tracking-label text-muted-foreground">
        Commits
      </h2>
      {comparison && comparison.commits.length > 0 ? (
        <ul className="mt-3 divide-y divide-border border border-border bg-card">
          {comparison.commits.map((c) => (
            <li key={c.oid} className="flex items-center gap-3 px-4 py-3 text-sm2">
              <GitCommitHorizontal className="size-4 shrink-0 text-muted-foreground" />
              <span className="min-w-0 flex-1 truncate">{commitTitle(c.message)}</span>
              <span className="shrink-0 text-caption text-muted-foreground">{c.author}</span>
              <Link href={`${base}/commit/${c.oid}`} className="shrink-0 font-mono text-caption text-primary underline-offset-4 hover:underline">
                {shortOid(c.oid)}
              </Link>
              <span className="shrink-0 text-caption text-muted-foreground">{whenSeconds(c.time)}</span>
            </li>
          ))}
        </ul>
      ) : (
        <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          {comparison ? "This branch adds nothing the base does not already have." : "The branches could not be read."}
        </p>
      )}

      <h2 className="mt-10 flex items-center gap-2 text-caption font-semibold uppercase tracking-label text-muted-foreground">
        Files changed
        {diff && diff.files.length > 0 && (
          <span className="font-mono normal-case tracking-normal">
            <span className="text-success">+{diff.additions}</span>{" "}
            <span className="text-destructive">−{diff.deletions}</span>
          </span>
        )}
      </h2>
      <div className="mt-3">
        {diff ? <DiffFiles diff={diff} base={base} /> : null}
      </div>

      {pr.state === "merged" && (
        <p className="mt-8 flex items-center gap-2 border border-primary/40 bg-primary/5 px-4 py-3 text-sm2">
          <GitMerge className="size-4 shrink-0 text-primary" />
          This change was merged into <code className="font-mono text-caption">{pr.base}</code>.
        </p>
      )}
    </section>
  );
}
