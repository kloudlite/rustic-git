import Link from "next/link";
import { Boxes, Camera, Code, Container, GitPullRequest, Settings, Tag } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { UserMenu } from "@/components/app/user-menu";
import { GlobalSearch } from "@/components/app/global-search";
import { ShellState } from "@/components/app/shell-context";
import { ShellCrumb, ShellTabs, type RepoTabSpec } from "@/components/app/shell-nav";
import { ownersFor } from "@/lib/owners";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { Session } from "@/lib/session";

/** The repo's tabs, as suffixes — which repo they belong to is a fact about the
 *  URL, and the shell reads that itself. */
const REPO_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Code", icon: <Code /> },
  { suffix: "/pulls", label: "Pull requests", icon: <GitPullRequest /> },
  // "Repo settings", not "Settings": the user, a team, a repo and an image each have a settings
  // page, and a tab that just says "Settings" leaves the person guessing which one they are on.
  { suffix: "/settings", label: "Repo settings", icon: <Settings />, end: true },
];

/** An image's tabs, same shape as `REPO_TABS` — which image they belong to is a
 *  fact about the URL, read by the shell itself. */
const IMAGE_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Details", icon: <Container /> },
  { suffix: "/tags", label: "Tags", icon: <Tag /> },
  { suffix: "/settings", label: "Image settings", icon: <Settings />, end: true },
];

/** An environment's tabs. `exact` on Services because its href is a prefix of Snapshots — the
 *  same rule Home follows in `sections`. */
const ENV_TABS: RepoTabSpec[] = [
  // "Live services", not "Services": these are the containers running RIGHT NOW, which is a
  // different thing from the service list a snapshot recorded.
  { suffix: "", label: "Live services", icon: <Boxes />, exact: true },
  { suffix: "/snapshots", label: "Snapshots", icon: <Camera /> },
  // Constant for an archived environment too: the page there is the Danger zone alone, and a tab
  // row that loses a tab reads as a page that failed to load.
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

            <GlobalSearch me={me} owners={owners} />
            <UserMenu name={session.user.name} email={session.user.email} />
          </div>

          <ShellTabs
            repoTabs={REPO_TABS}
            imageTabs={IMAGE_TABS}
            envTabs={ENV_TABS}
            me={me}
            className="mx-auto max-w-page px-5"
          />
        </header>

        <ScrollArea className="flex-1">{children}</ScrollArea>
      </div>
    </ShellState>
  );
}
