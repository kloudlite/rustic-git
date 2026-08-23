import Link from "next/link";
import { CircleDot, Code, Container, GitPullRequest, Settings, Tag } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { UserMenu } from "@/components/app/user-menu";
import { GlobalSearch } from "@/components/app/global-search";
import { ShellState } from "@/components/app/shell-context";
import { ShellCrumb, ShellTabs, type RepoTabSpec } from "@/components/app/shell-nav";
import { ownersFor } from "@/lib/owners";
import { apiToken } from "@/lib/api-token";
import { listRepos } from "@/lib/api";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { Session } from "@/lib/session";

/** The repo's tabs, as suffixes — which repo they belong to is a fact about the
 *  URL, and the shell reads that itself. */
const REPO_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Code", icon: <Code /> },
  { suffix: "/issues", label: "Issues", icon: <CircleDot /> },
  { suffix: "/pulls", label: "Pull requests", icon: <GitPullRequest /> },
  { suffix: "/settings", label: "Settings", icon: <Settings />, end: true },
];

/** An image's tabs, same shape as `REPO_TABS` — which image they belong to is a
 *  fact about the URL, read by the shell itself. */
const IMAGE_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Details", icon: <Container /> },
  { suffix: "/tags", label: "Tags", icon: <Tag /> },
  { suffix: "/settings", label: "Settings", icon: <Settings />, end: true },
];

/**
 * The chrome, mounted ONCE for every signed-in page.
 *
 * It is a layout and nothing renders a second one, because a tab row that is torn
 * down and rebuilt cannot animate — it can only reappear somewhere else. That is
 * also why neither the tabs nor the owner are passed in: a page being replaced
 * beneath the shell cannot hand it anything. The shell reads the URL and decides
 * for itself, which it can do because the names the namespace has spent are not
 * legal repo names. All this server component contributes is what the URL cannot
 * say: who is signed in, which namespaces they can act in, and what is in them.
 *
 * Chrome never gains a third row: anything deeper navigates inside the content.
 */
export async function AppShell({
  session,
  children,
}: {
  session: NonNullable<Session>;
  children: React.ReactNode;
}) {
  const me = session.user.owner;
  const owners = await ownersFor(session);
  // Every repo the person can jump to, for ⌘K. One list call per namespace, on
  // a full render only — client navigations keep this layout mounted.
  // ponytail: N calls per hard load; a single cross-owner list endpoint when teams grow
  const token = await apiToken();
  const lists = token ? await Promise.all(owners.map((o) => listRepos(token, o.slug))) : [];
  const repos = lists.flatMap((r) => (r.ok ? r.value : []));

  return (
    <ShellState>
      <div className="flex h-screen flex-col">
        {/* Chrome is a flex sibling of the scroll region, not sticky inside it: the
            header never scrolls, and the scrollbar belongs to the content alone. */}
        <header className="shrink-0 border-b border-border bg-card">
          <div className="mx-auto flex h-14 max-w-page items-center gap-3 px-6">
            <Link href="/" aria-label="kloudlite home" className="inline-flex">
              <Logo className="h-5" />
            </Link>
            <span className="text-muted-foreground/40" aria-hidden>/</span>

            <ShellCrumb me={me} owners={owners} />

            <div className="flex-1" />

            <GlobalSearch me={me} owners={owners} repos={repos} />
            <UserMenu name={session.user.name} email={session.user.email} />
          </div>

          <ShellTabs
            repoTabs={REPO_TABS}
            imageTabs={IMAGE_TABS}
            me={me}
            className="mx-auto max-w-page px-5"
          />
        </header>

        <ScrollArea className="flex-1">{children}</ScrollArea>
      </div>
    </ShellState>
  );
}
