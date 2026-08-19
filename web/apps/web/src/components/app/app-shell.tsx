import Link from "next/link";
import { ChevronsUpDown, CircleDot, Code, GitPullRequest, Play, Search, Settings } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { sections, settingsSection } from "@/components/app/sections";
import { UserMenu } from "@/components/app/user-menu";
import { NavTabs, type NavTab } from "@/components/app/nav-tabs";
import type { Session } from "@/lib/session";

/** What the tab row is about. At the org, it lists the org's sections; inside a
 *  repo it lists the repo's. The breadcrumb grows one segment to say which. Chrome
 *  never gains a third row: anything deeper navigates inside the content. */
export type ShellContext =
  | { kind: "org" }
  | { kind: "repo"; name: string; visibility: "public" | "private" };

function repoTabs(owner: string, repo: string): NavTab[] {
  const base = `/${owner}/${repo}`;
  return [
    { href: base, label: "Code", icon: <Code /> },
    { href: `${base}/issues`, label: "Issues", icon: <CircleDot /> },
    { href: `${base}/pulls`, label: "Pull requests", icon: <GitPullRequest /> },
    { href: `${base}/actions`, label: "Actions", icon: <Play /> },
    { href: `${base}/settings`, label: "Settings", icon: <Settings />, end: true },
  ];
}

function orgTabs(owner: string): NavTab[] {
  return [...sections(owner), settingsSection(owner)].map(({ href, label, icon: Icon }, i, all) => ({
    href,
    label,
    icon: <Icon />,
    end: i === all.length - 1,
  }));
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

        <NavTabs
          tabs={tabs}
          active={active}
          back={context.kind === "repo" ? { href: `/${owner}`, label: `Back to ${owner}` } : undefined}
          className="mx-auto max-w-page px-5"
          aria-label={context.kind === "repo" ? context.name : "Sections"}
        />
      </header>

      {children}
    </div>
  );
}
