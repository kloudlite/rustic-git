import { cn } from "@/lib/utils";

/** Every content block on every superadmin screen is one of these — an 11 px uppercase eyebrow, a
 *  13 px title, an optional count chip and a right toolbar, over a padded body (design README:
 *  "every content block is a section with the same anatomy"). v1's unlabelled bordered boxes are
 *  what this replaces, so nothing outside `ui/` should draw its own card border again.
 *
 *  `bare` drops the body padding for a section whose body is a `DataTable`: the table draws its
 *  own row padding, and doubling them puts the header out of line with the rows. */
export function Section({
  eyebrow,
  title,
  count,
  toolbar,
  bare = false,
  className,
  children,
}: {
  eyebrow: string;
  title: string;
  count?: string | number;
  toolbar?: React.ReactNode;
  bare?: boolean;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <section className={cn("border border-border bg-card", className)}>
      <header className="flex min-h-12 items-center gap-3 border-b border-border px-4 py-2">
        <div className="min-w-0">
          <p className="text-micro font-medium tracking-eyebrow text-muted-foreground uppercase">{eyebrow}</p>
          <h2 className="truncate text-sm2 font-semibold">{title}</h2>
        </div>
        {count !== undefined && (
          <span className="border border-border px-1.5 text-micro tabular-nums text-muted-foreground">{count}</span>
        )}
        <div className="flex-1" />
        {toolbar && <div className="flex shrink-0 items-center gap-2">{toolbar}</div>}
      </header>
      <div className={bare ? undefined : "p-4"}>{children}</div>
    </section>
  );
}
