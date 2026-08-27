import { GitMerge, GitPullRequestClosed } from "lucide-react";
import { Initials } from "@/components/app/initials";
import { PullActions, CommentBox } from "@/components/repo/pull-actions";
import { PullSidebar } from "@/components/repo/pull-sidebar";
import { displayName } from "@/lib/person";
import { whenSeconds } from "@/lib/time";
import type { ApiComment, ApiPull } from "@/lib/api";

/** A comment or the PR body. The avatar sits INSIDE the card header rather than
 *  floating beside it, so a one-line comment is a one-line card. */
function Comment({ author, body, said, when }: { author: string; body: string; said: string; when?: string }) {
  const name = displayName(author);
  return (
    <div className="border border-border bg-card">
      <div className="flex items-center gap-2 border-b border-border px-4 py-2.5 text-caption">
        <Initials name={name} size={6} />
        <span className="font-medium text-foreground">{name}</span>
        <span className="text-muted-foreground">{said}{when ? ` ${when}` : ""}</span>
      </div>
      <div className="whitespace-pre-line px-4 py-3 text-sm2 leading-relaxed text-foreground/90">{body}</div>
    </div>
  );
}

/** The api sends either a unix timestamp or a mongo `$date`; only the first can be
 *  turned into words here, and a comment with the other simply carries no time. */
function commentedAt(at: ApiComment["at"]) {
  return typeof at === "number" ? whenSeconds(at) : undefined;
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
  const comments = pull.comments ?? [];
  return (
    <div className="mt-6 grid gap-10 lg:grid-cols-overview">
      <section className="grid min-w-0 gap-8">
        {pull.body ? (
          <Comment author={pull.author} body={pull.body} said="opened this pull request" />
        ) : (
          <p className="border border-dashed border-border px-4 py-6 text-center text-sm2 text-muted-foreground">
            No description.
          </p>
        )}

        {comments.length > 0 && (
          <div className="grid gap-6">
            {comments.map((c, i) => (
              <Comment key={i} author={c.author} body={c.body} said="commented" when={commentedAt(c.at)} />
            ))}
          </div>
        )}

        {/* The end of the thread, drawn as a thread: a connector down from the last
            card to the event that closed it. Who did it is not stored, so it is not
            claimed — the sentence has no actor rather than a guessed one. */}
        {pull.state !== "open" && (
          <div className="-mt-6">
            <div className="ml-[15px] h-4 border-l border-border" />
            <p className="flex items-center gap-2 text-sm2 text-muted-foreground">
              {pull.state === "merged" ? (
                <GitMerge className="size-4 text-primary" />
              ) : (
                <GitPullRequestClosed className="size-4 text-destructive" />
              )}
              This pull request was {pull.state === "merged" ? "merged" : "closed"}.
            </p>
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
