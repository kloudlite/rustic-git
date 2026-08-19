import Link from "next/link";
import {
  ChevronsUpDown, CircleDot, Code, GitPullRequest, Play, Search, Settings, type LucideIcon,
} from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { sections, settingsSection } from "@/components/app/sections";
import { UserMenu } from "@/components/app/user-menu";
import { cn } from "@/lib/utils";
import type { Session } from "@/lib/session";

/** What the tab row is about. At the org, it lists the org's sections; inside a
 *  repo it lists the repo's. The breadcrumb grows one segment to say which. Chrome
 *  never gains a third row: anything deeper navigates inside the content. */
export type ShellContext =
  | { kind: "org" }
  | { kind: "repo"; name: string; visibility: "public" | "private" };

type Tab = { href: string; label: string; icon: LucideIcon; end?: boolean };

function repoTabs(owner: string, repo: string): Tab[] {
  const base = `/${owner}/${repo}`;
  return [
    { href: base, label: "Code", icon: Code },
    { href: `${base}/issues`, label: "Issues", icon: CircleDot },
    { href: `${base}/pulls`, label: "Pull requests", icon: GitPullRequest },
    { href: `${base}/actions`, label: "Actions", icon: Play },
    { href: `${base}/settings`, label: "Settings", icon: Settings, end: true },
  ];
}

function orgTabs(owner: string): Tab[] {
  return [...sections(owner), { ...settingsSection(owner), end: true }];
}

export function AppShell({
  session,
  active,
  context = { kind: "org" },
  children,
}: {
  session: NonNullable<Session>;
  active?: string;
  context?: ShellContext;
  children: React.ReactNode;
}) {
  const owner = session.user.owner;
  const tabs = context.kind === "repo" ? repoTabs(owner, context.name) : orgTabs(owner);

  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-40 border-b border-border bg-background">
        <div className="mx-auto flex h-14 max-w-page items-center gap-3 px-6">
          <Link href="/" aria-label="kloudlite home" className="inline-flex">
            <Logo className="h-5" />
          </Link>
          <span className="text-muted-foreground/40" aria-hidden>/</span>

          {context.kind === "org" ? (
            <button
              type="button"
              className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
            >
              <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
              {owner}
              <ChevronsUpDown className="size-3.5 text-muted-foreground" />
            </button>
          ) : (
            <>
              <Link href={`/${owner}`} className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground">
                <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
                {owner}
              </Link>
              <span className="text-muted-foreground/40" aria-hidden>/</span>
              <Link href={`/${owner}/${context.name}`} className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted">
                {context.name}
                <span className="border border-border px-1.5 py-px text-micro font-medium text-muted-foreground">
                  {context.visibility}
                </span>
              </Link>
            </>
          )}

          <div className="flex-1" />

          <button
            type="button"
            className="hidden h-8 w-64 items-center gap-2 border border-edge px-2.5 text-left text-sm2 text-muted-foreground transition-colors hover:bg-muted md:flex"
          >
            <Search className="size-3.5" />
            Search
            <kbd className="ml-auto border border-border px-1 font-mono text-micro leading-4">⌘K</kbd>
          </button>
          <UserMenu name={session.user.name} email={session.user.email} />
        </div>

        <nav className="mx-auto -mb-px flex max-w-page items-stretch gap-2 px-6" aria-label={context.kind === "repo" ? context.name : "Sections"}>
          {tabs.map(({ href, label, icon: Icon, end }) => {
            const isActive = active === label;
            return (
              <Link
                key={href}
                href={href}
                aria-current={isActive ? "page" : undefined}
                className={cn(
                  "flex h-10 items-center gap-2 whitespace-nowrap border-b-2 px-3 text-sm2 transition-colors",
                  end && "ml-auto",
                  isActive
                    ? "border-primary font-medium text-foreground"
                    : "border-transparent text-muted-foreground hover:border-border hover:text-foreground",
                )}
              >
                <Icon className="size-4" />
                {label}
              </Link>
            );
          })}
        </nav>
      </header>

      {children}
    </div>
  );
}
