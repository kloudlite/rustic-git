import Link from "next/link";
import { CircleDot, Code, Container, GitPullRequest, Settings, Tag } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { sections, settingsSection } from "@/components/app/sections";
import { UserMenu } from "@/components/app/user-menu";
import { GlobalSearch } from "@/components/app/global-search";
import { TeamSwitcher } from "@/components/app/team-switcher";
import { ShellState } from "@/components/app/shell-context";
import { ShellCrumb, ShellTabs, type RepoTabSpec } from "@/components/app/shell-nav";
import { ownersFor } from "@/lib/owners";
import { ScrollArea } from "@/components/ui/scroll-area";
import { RESERVED } from "@/lib/reserved";
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
 * also why the tabs are not passed in: a page being replaced beneath the shell
 * cannot hand it anything. The shell reads the URL and decides for itself, which
 * it can do because the names the namespace has spent are not legal repo names.
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
  const owner = session.user.owner;
  const owners = await ownersFor(session);
  const code = sections(owner).find((s) => s.label === "Code Repos")!;
  const CodeIcon = code.icon;
  const registries = sections(owner).find((s) => s.label === "Container Images")!;
  const RegistriesIcon = registries.icon;

  const orgTabs = [...sections(owner), settingsSection(owner)].map(
    ({ href, label, icon: Icon }, i, all) => ({
      href,
      label,
      icon: <Icon />,
      end: i === all.length - 1,
    }),
  );

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

            <ShellCrumb
              reserved={RESERVED}
              switcher={<TeamSwitcher current={owner} owners={owners} />}
              section={
                <Link
                  href={code.href}
                  className="flex h-8 items-center gap-1.5 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                >
                  <CodeIcon className="size-3.5" />
                  {code.label}
                </Link>
              }
              imageSection={
                <Link
                  href={registries.href}
                  className="flex h-8 items-center gap-1.5 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                >
                  <RegistriesIcon className="size-3.5" />
                  {registries.label}
                </Link>
              }
            />

            <div className="flex-1" />

            <GlobalSearch owner={owner} owners={owners} />
            <UserMenu name={session.user.name} email={session.user.email} />
          </div>

          <ShellTabs
            orgTabs={orgTabs}
            repoTabs={REPO_TABS}
            imageTabs={IMAGE_TABS}
            reserved={RESERVED}
            className="mx-auto max-w-page px-5"
          />
        </header>

        <ScrollArea className="flex-1">{children}</ScrollArea>
      </div>
    </ShellState>
  );
}
