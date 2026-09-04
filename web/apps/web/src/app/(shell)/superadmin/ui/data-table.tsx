import { cn } from "@/lib/utils";

/** The table conventions of the whole place, in one file so no screen re-invents them: a sticky
 *  uppercase header, 40 px rows, a `bg-muted` hover ground, right-aligned tabular numbers, and row
 *  actions that only appear on the hovered row. */
export function DataTable({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <div className={cn("overflow-x-auto", className)}>
      <table className="w-full border-collapse text-sm2">{children}</table>
    </div>
  );
}

export function Th({
  children,
  numeric = false,
  className,
}: {
  children?: React.ReactNode;
  numeric?: boolean;
  className?: string;
}) {
  return (
    <th
      scope="col"
      className={cn(
        "sticky top-0 z-10 border-b border-border bg-card px-3 py-2 text-micro font-medium tracking-label text-muted-foreground uppercase",
        numeric ? "text-right tabular-nums" : "text-left",
        className,
      )}
    >
      {children}
    </th>
  );
}

export function Td({
  children,
  numeric = false,
  className,
}: {
  children?: React.ReactNode;
  numeric?: boolean;
  className?: string;
}) {
  return (
    <td className={cn("h-10 border-b border-border px-3", numeric && "text-right tabular-nums", className)}>
      {children}
    </td>
  );
}

/** `<tr>` with the hover ground and the group the row actions key off. */
export function Tr({ children, className }: { children: React.ReactNode; className?: string }) {
  return <tr className={cn("group/row hover:bg-muted", className)}>{children}</tr>;
}

/** Wrap a row's buttons in this: hidden until the row is hovered or something inside it has focus,
 *  so a table of forty nodes is not forty pairs of buttons competing with the data — and still
 *  reachable by keyboard, which `hidden` alone would not be. */
export function RowActions({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-end gap-2 opacity-0 transition-opacity group-hover/row:opacity-100 focus-within:opacity-100">
      {children}
    </div>
  );
}

/** One sentence, one action (spec §C). Never an empty bordered box. */
export function EmptyState({ children, action }: { children: React.ReactNode; action?: React.ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-3 px-4 py-10 text-center">
      <p className="text-sm2 text-muted-foreground">{children}</p>
      {action}
    </div>
  );
}
