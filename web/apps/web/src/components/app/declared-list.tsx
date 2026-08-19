import Link from "next/link";
import { FileCode } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Input } from "@/components/ui/input";
import { Search } from "lucide-react";
import type { Session } from "@/lib/session";
import type { Declared } from "@/lib/mock";

/** The pointer every team-level item carries: the repo and file that declare it.
 *  There is no edit button on these pages on purpose — the file is the edit. */
export function Source({ owner, source }: { owner: string; source: Declared }) {
  return (
    <Link
      href={`/${owner}/${source.repo}/blob/${source.path}`}
      className="inline-flex min-w-0 items-center gap-1.5 font-mono text-caption text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline"
      title={`Defined in ${source.repo}/${source.path}`}
    >
      <FileCode className="size-3.5 shrink-0" />
      <span className="truncate">{source.repo}/{source.path}</span>
    </Link>
  );
}

/** Frame for the three declared-as-code sections: same toolbar, same explanation
 *  of where the items come from, then the section's own list. */
export function DeclaredPage({
  session,
  active,
  filterLabel,
  dir,
  count,
  children,
}: {
  session: NonNullable<Session>;
  active: string;
  filterLabel: string;
  dir: string;
  count: number;
  children: React.ReactNode;
}) {
  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="flex flex-wrap items-center gap-3">
          <div className="relative w-full max-w-xs">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input placeholder={filterLabel} className="h-8 pl-8" aria-label={filterLabel} />
          </div>
          <p className="ml-auto text-sm2 text-muted-foreground">
            {count} across the team · declared in{" "}
            <Link href={`/${session.user.owner}/.kloudlite/tree/${dir}`} className="font-mono text-caption text-foreground underline-offset-4 hover:underline">.kloudlite/{dir}</Link>
          </p>
        </div>
        <div className="mt-5">{children}</div>
      </main>
    </AppShell>
  );
}
