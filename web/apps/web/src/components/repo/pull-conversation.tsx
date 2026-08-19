import Link from "next/link";
import { Check, CircleCheck, CircleX, GitCommitHorizontal, GitMerge, MessageSquare, Tag, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { PULL, REPO, type TimelineEvent } from "@/lib/mock-repo";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Textarea } from "@/components/ui/textarea";
import { Initials } from "@/components/app/initials";

function Avatar({ login, className }: { login: string; className?: string }) {
  return <Initials name={login} size={7} className={cn("shrink-0", className)} />;
}

/** A comment or the PR body: author line, then prose in a bordered box. */
function Comment({ author, when, body, tone }: { author: string; when: string; body: string; tone?: "approved" | "changes_requested" }) {
  return (
    <div className="flex gap-3">
      <Avatar login={author} />
      <div className={cn("min-w-0 flex-1 border", tone === "approved" ? "border-success/40" : tone === "changes_requested" ? "border-destructive/40" : "border-border")}>
        <div className={cn("flex items-center gap-2 border-b px-4 py-2 text-caption",
          tone === "approved" ? "border-success/40 bg-success/10" : tone === "changes_requested" ? "border-destructive/40 bg-destructive/10" : "border-border bg-muted/40")}>
          <span className="font-medium text-foreground">{author}</span>
          <span className="text-muted-foreground">
            {tone === "approved" ? "approved these changes" : tone === "changes_requested" ? "requested changes" : "commented"} {when}
          </span>
        </div>
        <div className="whitespace-pre-line px-4 py-3 text-sm2 leading-relaxed text-foreground/90">{body}</div>
      </div>
    </div>
  );
}

/** A one-line event: something happened, said in a sentence with an icon. */
function Event({ icon: Icon, tone, children }: { icon: typeof Check; tone?: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-3 py-1 text-sm2 text-muted-foreground">
      <span className={cn("flex size-7 shrink-0 items-center justify-center bg-muted", tone)}><Icon className="size-4" /></span>
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

function Timeline({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <div className="grid gap-6">
      {PULL.timeline.map((t: TimelineEvent) => {
        if (t.kind === "comment") return <Comment key={t.id} author={t.author} when={t.when} body={t.body} />;
        if (t.kind === "review") return t.body
          ? <Comment key={t.id} author={t.author} when={t.when} body={t.body} tone={t.state === "commented" ? undefined : t.state} />
          : <Event key={t.id} icon={Check} tone="text-success"><span className="font-medium text-foreground">{t.author}</span> approved {t.when}</Event>;
        if (t.kind === "checks") return (
          <Event key={t.id} icon={t.status === "passing" ? CircleCheck : CircleX} tone={t.status === "passing" ? "text-success" : "text-destructive"}>
            {t.detail} <span className="text-muted-foreground/70">· {t.when}</span>
          </Event>
        );
        if (t.kind === "label") return (
          <Event key={t.id} icon={Tag}>
            <span className="font-medium text-foreground">{t.author}</span> added{" "}
            <Badge variant="outline" className="text-foreground">{t.label}</Badge> {t.when}
          </Event>
        );
        return (
          <div key={t.id}>
            <Event icon={GitCommitHorizontal}>
              <span className="font-medium text-foreground">{t.author}</span> pushed {t.commits.length} commits {t.when}
            </Event>
            <ul className="ml-10 mt-1 grid gap-1 border-l border-border pl-3.5">
              {t.commits.map((c) => (
                <li key={c.sha} className="flex items-baseline gap-2.5 text-caption">
                  <Link href={`${base}/commit/${c.sha}`} className="shrink-0 font-mono text-primary underline-offset-4 hover:underline">{c.sha}</Link>
                  <span className="truncate text-muted-foreground">{c.message}</span>
                </li>
              ))}
            </ul>
          </div>
        );
      })}
    </div>
  );
}

/** The decision box. States the three things that gate a merge — checks, reviews,
 *  conflicts — each with its own verdict, then the action. */
function MergeBox() {
  const approved = PULL.reviewers.filter((r) => r.state === "approved").length;
  const allChecks = PULL.checks.every((c) => c.status === "passing");
  return (
    <div className="border border-border">
      <ul className="divide-y divide-border">
        <li className="flex items-center gap-3 px-4 py-3">
          {allChecks ? <CircleCheck className="size-4 text-success" /> : <CircleX className="size-4 text-destructive" />}
          <div className="min-w-0 flex-1">
            <div className="text-sm2 font-medium">All checks have passed</div>
            <div className="text-caption text-muted-foreground">{PULL.checks.length} successful checks</div>
          </div>
        </li>
        <li className="flex items-center gap-3 px-4 py-3">
          <CircleCheck className="size-4 text-success" />
          <div className="min-w-0 flex-1">
            <div className="text-sm2 font-medium">Changes approved</div>
            <div className="text-caption text-muted-foreground">{approved} approving review</div>
          </div>
        </li>
        <li className="flex items-center gap-3 px-4 py-3">
          <CircleCheck className="size-4 text-success" />
          <div className="min-w-0 flex-1">
            <div className="text-sm2 font-medium">No conflicts with the base branch</div>
            <div className="text-caption text-muted-foreground">Merging can be performed automatically</div>
          </div>
        </li>
      </ul>
      <div className="flex flex-wrap items-center gap-3 border-t border-border bg-muted/40 px-4 py-3">
        <Button className="bg-success text-primary-foreground hover:bg-success/90"><GitMerge />Squash and merge</Button>
        <span className="text-caption text-muted-foreground">2 commits → 1 on {PULL.base}</span>
      </div>
    </div>
  );
}

export function PullConversation({ owner }: { owner: string }) {
  return (
    <div className="mt-6 grid gap-10 lg:grid-cols-overview">
      <section className="grid gap-8">
        <Comment author={PULL.author} when={PULL.when} body={PULL.body} />
        <Timeline owner={owner} />
        <MergeBox />
        <div className="flex gap-3">
          <Avatar login="karthik" />
          <div className="min-w-0 flex-1 border border-border">
            <Textarea
              rows={3}
              placeholder="Leave a comment"
              className="min-h-0 resize-y rounded-none border-0 px-4 py-3 focus-visible:ring-0"
            />
            <div className="flex items-center justify-end gap-2 border-t border-border bg-muted/40 px-3 py-2">
              <Button variant="outline" className="border-edge hover:border-edge-hover"><X />Close pull request</Button>
              <Button><MessageSquare />Comment</Button>
            </div>
          </div>
        </div>
      </section>

      <aside className="grid content-start gap-6 text-sm2">
        <div>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Reviewers</h2>
          <ul className="mt-2 grid gap-2">
            {PULL.reviewers.map((r) => (
              <li key={r.login} className="flex items-center gap-2">
                <Initials name={r.login} size={6} />
                <span className="flex-1 font-medium">{r.login}</span>
                {r.state === "approved"
                  ? <Check className="size-4 text-success" aria-label="Approved" />
                  : <span className="text-caption text-muted-foreground">pending</span>}
              </li>
            ))}
          </ul>
        </div>
        <div>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Labels</h2>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {PULL.labels.map((l) => <Badge key={l} variant="outline">{l}</Badge>)}
          </div>
        </div>
        <div>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Linked issues</h2>
          <ul className="mt-2 grid gap-1.5">
            {PULL.linked.map((i) => (
              <li key={i.number}>
                <Link href={`/${owner}/${REPO.name}/issues/${i.number}`} className="underline-offset-4 hover:underline">
                  <span className="text-muted-foreground">#{i.number}</span> {i.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
        <div>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Checks</h2>
          <ul className="mt-2 grid gap-1.5">
            {PULL.checks.map((c) => (
              <li key={c.name} className="flex items-center gap-2">
                {c.status === "passing" ? <CircleCheck className="size-3.5 text-success" /> : <CircleX className="size-3.5 text-destructive" />}
                <span className="flex-1 font-mono text-caption">{c.name}</span>
                <span className="text-caption text-muted-foreground">{c.duration}</span>
              </li>
            ))}
          </ul>
        </div>
      </aside>
    </div>
  );
}
