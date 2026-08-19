import Link from "next/link";
import { RefPicker } from "@/components/repo/ref-picker";
import { CopyButton } from "@/components/repo/copy-button";
import { Initials } from "@/components/app/initials";
import { defaultBranch, log, refs, shortOid, shortRef } from "@/lib/browse";
import { commitTitle, dayBucket } from "@/components/repo/commit-meta";
import { whenSeconds } from "@/lib/time";
import type { Commit } from "@/lib/browse";

const PAGE = 40;

/** History of a ref, newest first, grouped by the day each commit was made.
 *
 *  Paged by CURSOR, not by offset: `?from=<oid>` starts the page at that commit
 *  and walks its first parents. An offset would re-walk the history from the tip
 *  on every page and would drift the moment someone pushed, showing a commit twice
 *  or not at all.
 *
 *  No file tree here: a commit belongs to no branch, and the tree is a fact about
 *  a ref rather than about a list of commits. */
export async function CommitsView({
  token,
  owner,
  repo,
  refName,
  from,
}: {
  token: string;
  owner: string;
  repo: string;
  refName?: string;
  from?: string;
}) {
  const base = `/${owner}/${repo}`;
  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);

  const fallback = defaultBranch(all.value);
  const head = (refName && all.value.find((r) => shortRef(r.name) === refName)) || fallback;
  if (!head) {
    return <p className="text-sm2 text-muted-foreground">This repo has no commits yet.</p>;
  }

  // One extra, so "is there a next page" is a fact rather than a guess that shows
  // an empty page at the end of history.
  const page = await log(token, owner, repo, from ?? head.oid, PAGE + 1);
  if (!page.ok) throw new Error(page.message);
  const commits = page.value.slice(0, PAGE);
  const next = page.value.length > PAGE ? page.value[PAGE].oid : undefined;

  const q = refName ? `ref=${encodeURIComponent(refName)}` : "";
  const days: { day: string; list: Commit[] }[] = [];
  for (const c of commits) {
    const day = dayBucket(c.time);
    const at = days.at(-1);
    if (at?.day === day) at.list.push(c);
    else days.push({ day, list: [c] });
  }

  return (
    <section>
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker
          current={shortRef(head.name)}
          defaultBranch={fallback ? shortRef(fallback.name) : undefined}
          branches={all.value.filter((r) => r.kind === "branch").map((r) => shortRef(r.name))}
          tags={all.value.filter((r) => r.kind === "tag").map((r) => shortRef(r.name))}
          base={`${base}/commits`}
        />
        {from && (
          <Link href={`${base}/commits${q ? `?${q}` : ""}`} className="text-sm2 text-primary underline-offset-4 hover:underline">
            Back to the latest
          </Link>
        )}
      </div>

      <div className="mt-6 grid gap-8">
        {days.map(({ day, list }) => (
          <div key={day}>
            <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">{day}</h2>
            <ul className="mt-3 divide-y divide-border border border-border bg-card">
              {list.map((c) => (
                <li key={c.oid} className="flex items-center gap-4 px-5 py-3.5">
                  <Initials name={c.author} size={7} />
                  <div className="min-w-0 flex-1">
                    <Link href={`${base}/commit/${c.oid}`} className="block truncate text-sm2 font-medium underline-offset-4 hover:underline">
                      {commitTitle(c.message)}
                    </Link>
                    <p className="mt-1 text-caption text-muted-foreground">
                      <span className="font-medium text-foreground/80">{c.author}</span> committed{" "}
                      <span title={new Date(c.time * 1000).toISOString()}>{whenSeconds(c.time)}</span>
                    </p>
                  </div>
                  <div className="flex shrink-0 items-center border border-edge">
                    <Link href={`${base}/commit/${c.oid}`} className="px-2.5 py-1 font-mono text-caption text-primary hover:bg-muted">
                      {shortOid(c.oid)}
                    </Link>
                    <CopyButton value={c.oid} label="Copy the full sha" className="border-l border-edge" />
                  </div>
                </li>
              ))}
            </ul>
          </div>
        ))}
        {commits.length === 0 && (
          <p className="border border-border bg-card px-5 py-10 text-center text-sm2 text-muted-foreground">
            No commits here.
          </p>
        )}
      </div>

      {next && (
        <div className="mt-6">
          <Link
            href={`${base}/commits?from=${next}${q ? `&${q}` : ""}`}
            className="inline-flex h-9 items-center border border-edge px-4 text-sm2 font-medium transition-colors hover:bg-muted"
          >
            Older
          </Link>
        </div>
      )}
    </section>
  );
}
