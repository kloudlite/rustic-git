import Link from "next/link";
import { ChevronsUpDown, CircleDot, Code, GitPullRequest, Search, Settings } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { sections, settingsSection } from "@/components/app/sections";
import { UserMenu } from "@/components/app/user-menu";
import { NavTabs, type NavTab } from "@/components/app/nav-tabs";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { Session } from "@/lib/session";
import { Badge } from "@/components/ui/badge";
import { Kbd } from "@/components/ui/kbd";
import { Button } from "@/components/ui/button";

/** What the tab row is about. At the org, it lists the org's sections; inside a
 *  repo it lists the repo's. The breadcrumb grows one segment to say which. Chrome
 *  never gains a third row: anything deeper navigates inside the content. */
export type ShellContext =
  | { kind: "org" }
  | { kind: "repo"; name: string; visibility: "public" | "private" };

/** Which section an item belongs to, so the breadcrumb can say so. A repo, a
 *  workspace and an environment can share a name; the section is what tells
 *  them apart, and it is also the list the item came from. */
function sectionOf(context: ShellContext, owner: string) {
  if (context.kind === "repo") return sections(owner).find((s) => s.label === "Code Repos")!;
  return null;
}

function repoTabs(owner: string, repo: string): NavTab[] {
  const base = `/${owner}/${repo}`;
  return [
    { href: base, label: "Code", icon: <Code /> },
    { href: `${base}/issues`, label: "Issues", icon: <CircleDot /> },
    { href: `${base}/pulls`, label: "Pull requests", icon: <GitPullRequest /> },
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
    <div className="flex h-screen flex-col">
      {/* Chrome is a flex sibling of the scroll region, not sticky inside it: the
          header never scrolls, and the scrollbar belongs to the content alone. */}
      <header className="shrink-0 border-b border-border bg-card">
        <div className="mx-auto flex h-14 max-w-page items-center gap-3 px-6">
          <Link href="/" aria-label="kloudlite home" className="inline-flex">
            <Logo className="h-5" />
          </Link>
          <span className="text-muted-foreground/40" aria-hidden>/</span>

          {context.kind === "org" ? (
            <Button variant="ghost" className="px-2">
              <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
              {owner}
              <ChevronsUpDown className="text-muted-foreground" />
            </Button>
          ) : (
            <>
              <Link href="/" className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground">
                <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
                {owner}
              </Link>
              <span className="text-muted-foreground/40" aria-hidden>/</span>
              {(() => {
                const section = sectionOf(context, owner)!;
                const Icon = section.icon;
                return (
                  <Link href={section.href} className="flex h-8 items-center gap-1.5 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground">
                    <Icon className="size-3.5" />
                    {section.label}
                  </Link>
                );
              })()}
              <span className="text-muted-foreground/40" aria-hidden>/</span>
              <Link href={`/${owner}/${context.name}`} className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted">
                {context.name}
                <Badge variant="outline">
                  {context.visibility}
                </Badge>
              </Link>
            </>
          )}

          <div className="flex-1" />

          <Button
            variant="outline"
            className="hidden w-64 justify-start border-edge font-normal text-muted-foreground hover:border-edge-hover hover:text-foreground md:flex"
          >
            <Search />
            Search
            <Kbd className="ml-auto">⌘K</Kbd>
          </Button>
          <UserMenu name={session.user.name} email={session.user.email} />
        </div>

        <NavTabs
          tabs={tabs}
          active={active}
          back={context.kind === "repo" ? { href: `/${owner}`, label: "Repos" } : undefined}
          className="mx-auto max-w-page px-5"
          aria-label={context.kind === "repo" ? context.name : "Sections"}
        />
      </header>

      <ScrollArea className="flex-1">{children}</ScrollArea>
    </div>
  );
}
