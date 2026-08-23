import { Initials } from "@/components/app/initials";
import { PullActions, CommentBox } from "@/components/repo/pull-actions";
import { PullSidebar } from "@/components/repo/pull-sidebar";
import type { ApiPull } from "@/lib/api";

/** A comment or the PR body: author line, then prose in a bordered box. */
function Comment({ author, body, when }: { author: string; body: string; when?: string }) {
  return (
    <div className="flex gap-3">
      <Initials name={author} size={7} />
      <div className="min-w-0 flex-1 border border-border">
        <div className="flex items-center gap-2 border-b border-border bg-muted/40 px-4 py-2 text-caption">
          <span className="font-medium text-foreground">{author}</span>
          <span className="text-muted-foreground">commented{when ? ` ${when}` : ""}</span>
        </div>
        <div className="whitespace-pre-line px-4 py-3 text-sm2 leading-relaxed text-foreground/90">{body}</div>
      </div>
    </div>
  );
}

/** The conversation: what the change says of itself, what people said back, and
 *  the decision box that gates the merge. Two columns, as designed — the rail
 *  carries the things that are about the change rather than in it. */
export function PullConversation({
  owner,
  repo,
  pull,
}: {
  owner: string;
  repo: string;
  pull: ApiPull;
}) {
  return (
    <div className="mt-6 grid gap-10 lg:grid-cols-overview">
      <section className="grid min-w-0 gap-8">
        {pull.body ? (
          <Comment author={pull.author} body={pull.body} />
        ) : (
          <p className="border border-dashed border-border px-4 py-6 text-center text-sm2 text-muted-foreground">
            No description.
          </p>
        )}

        {(pull.comments ?? []).length > 0 && (
          <div className="grid gap-6">
            {(pull.comments ?? []).map((c, i) => (
              <Comment key={i} author={c.author} body={c.body} />
            ))}
          </div>
        )}

        <PullActions
          owner={owner}
          repo={repo}
          number={pull.number}
          state={pull.state}
          baseBranch={pull.base}
          mergeability={pull.mergeability}
          job={pull.merge}
        />

        <CommentBox owner={owner} repo={repo} number={pull.number} />
      </section>

      <PullSidebar />
    </div>
  );
}
