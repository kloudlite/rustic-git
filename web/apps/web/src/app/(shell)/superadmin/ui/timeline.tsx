import { initials } from "@/lib/console";

/** The activity/audit timeline: a left time rail, actor initials, one sentence per row. Rows are
 *  ordered by the caller — a timeline never sorts, because "newest first" and "oldest open first"
 *  are decisions the page makes, not the chrome. */
export function Timeline({ children }: { children: React.ReactNode }) {
  return <ol className="flex flex-col">{children}</ol>;
}

export function TimelineRow({
  at,
  actor,
  children,
  note,
}: {
  at: string;
  actor: string;
  children: React.ReactNode;
  note?: string | null;
}) {
  return (
    <li className="flex gap-3 border-b border-border py-2 last:border-0">
      <span className="w-20 shrink-0 pt-0.5 text-caption tabular-nums text-muted-foreground">{at}</span>
      <span className="flex size-5 shrink-0 items-center justify-center bg-muted text-micro font-medium text-muted-foreground">
        {initials(actor)}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-sm2">{children}</p>
        {note && <p className="truncate text-caption text-muted-foreground">&ldquo;{note}&rdquo;</p>}
      </div>
    </li>
  );
}
