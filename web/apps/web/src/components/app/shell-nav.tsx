"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { NavTabs } from "@/components/app/nav-tabs";
import { useRepoMeta } from "@/components/app/shell-context";
import { sections, settingsSection } from "@/components/app/sections";
import { TeamSwitcher, type SwitcherOwner } from "@/components/app/team-switcher";
import { RESERVED } from "@/lib/reserved";
import { Badge } from "@/components/ui/badge";

/** A repo tab, as the shell is given it: the icon is already rendered, because a
 *  component cannot cross from the server into here, and the href is a suffix
 *  because which repo it belongs to is only known from the URL. */
export type RepoTabSpec = { suffix: string; label: string; icon: React.ReactNode; end?: boolean; exact?: boolean };

/** Pages that hang off the root rather than off an owner. A URL starting with one
 *  of these names nobody's namespace, so the chrome shows the person's own. */
// ponytail: a person whose handle is one of these words would see the wrong crumb; `settings` is already refused as a handle, the other two are not
const ROOT_PAGES = ["settings", "new-repo", "new-team", "invite"];

/** Where the URL is, in the terms the chrome cares about.
 *
 *  `/{owner}/{x}` is unambiguous because the names the namespace has spent —
 *  settings, activity, ci, and the rest — cannot be repo names; repo creation
 *  refuses them. So the second segment names a repo or it names a section, and
 *  the chrome can tell which without asking anyone.
 *
 *  `/{owner}/registries/{image}` is a third place, one level deeper: `registries`
 *  is itself a reserved section (the Container Images list), so it is already
 *  caught by the `repo` branch above at two segments — the third segment is what
 *  tells an image page apart from the list page it hangs off of.
 *
 *  The owner is the first segment whenever there is one. The shell is a layout
 *  that stays mounted across navigations, so it cannot be handed the owner by a
 *  page; reading the URL is the only way a team's pages get the team's chrome. */
export function place(pathname: string, me: string) {
  const parts = pathname.split("/").filter(Boolean);
  const owner = parts[0] && !ROOT_PAGES.includes(parts[0]) ? parts[0] : me;
  if (parts.length >= 3 && parts[1] === "registries") {
    return { kind: "image" as const, owner, image: parts[2] };
  }
  // Same shape one level deeper, for the same reason: `environments` is a reserved section, so
  // two segments is the LIST and three is one environment — which has its own tabs and its own
  // crumb, exactly as a repo or an image does.
  if (parts.length >= 3 && parts[1] === "environments") {
    return { kind: "env" as const, owner, env: parts[2] };
  }
  if (parts.length >= 2 && !(RESERVED as readonly string[]).includes(parts[1])) {
    return { kind: "repo" as const, owner, repo: parts[1] };
  }
  return { kind: "org" as const, owner };
}

export function useOwner(me: string) {
  return place(usePathname(), me).owner;
}

export function ShellTabs({
  repoTabs,
  imageTabs,
  envTabs,
  me,
  className,
}: {
  repoTabs: RepoTabSpec[];
  imageTabs: RepoTabSpec[];
  envTabs: RepoTabSpec[];
  /** The signed-in person's own handle: what the chrome falls back to at `/`. */
  me: string;
  className?: string;
}) {
  const at = place(usePathname(), me);
  const meta = useRepoMeta();
  if (at.kind === "org") {
    // A person's own namespace is not a team: it has no members, no roles and nothing to
    // rename, so it gets no Settings tab. Their settings are at /settings, off the avatar.
    // Showing the tab here was what made a fresh account look like it came with a team.
    const own = at.owner === me;
    const tabs = [...sections(at.owner), ...(own ? [] : [settingsSection(at.owner)])].map(
      ({ href, label, icon: Icon, exact }, i, all) => ({
        href,
        label,
        icon: <Icon />,
        exact,
        end: !own && i === all.length - 1,
      }),
    );
    return <NavTabs tabs={tabs} className={className} aria-label="Sections" />;
  }
  if (at.kind === "image") {
    const base = `/${at.owner}/registries/${at.image}`;
    return (
      <NavTabs
        tabs={imageTabs.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end }))}
        back={{ href: `/${at.owner}/registries`, label: "Container Images" }}
        className={className}
        aria-label={at.image}
      />
    );
  }
  if (at.kind === "env") {
    const base = `/${at.owner}/environments/${at.env}`;
    // An ARCHIVED environment runs nothing, so it gets no Live services tab — the snapshots are
    // all that is left of it, and a tab whose only content is "there is nothing here" is a tab
    // that wastes a click. Until the layout says which it is, both show.
    const shown = meta?.archived ? envTabs.filter((t) => t.suffix !== "") : envTabs;
    return (
      <NavTabs
        tabs={shown.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end, exact: t.exact }))}
        back={{ href: `/${at.owner}/environments`, label: "Environments" }}
        className={className}
        aria-label={at.env}
      />
    );
  }
  const base = `/${at.owner}/${at.repo}`;
  return (
    <NavTabs
      tabs={repoTabs.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end }))}
      back={{ href: `/${at.owner}/repos`, label: "Repos" }}
      className={className}
      aria-label={at.repo}
    />
  );
}

/** The list a repo or an image came from, as a crumb segment. */
function SectionLink({ owner, label }: { owner: string; label: "Code Repos" | "Container Images" | "Environments" }) {
  const s = sections(owner).find((x) => x.label === label)!;
  const Icon = s.icon;
  return (
    <Link
      href={s.href}
      className="flex h-8 items-center gap-1.5 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
    >
      <Icon className="size-3.5" />
      {s.label}
    </Link>
  );
}

function OwnerLink({ owner }: { owner: string }) {
  return (
    <Link
      href={`/${owner}`}
      className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
    >
      <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
      {owner}
    </Link>
  );
}

/** The breadcrumb, which grows a segment inside a repo or an image. */
export function ShellCrumb({ me, owners }: { me: string; owners: SwitcherOwner[] }) {
  const at = place(usePathname(), me);
  const meta = useRepoMeta();
  if (at.kind === "org") return <TeamSwitcher current={at.owner} owners={owners} />;

  const sep = <span className="text-muted-foreground/40" aria-hidden>/</span>;
  if (at.kind === "env") {
    return (
      <>
        <OwnerLink owner={at.owner} />
        {sep}
        <SectionLink owner={at.owner} label="Environments" />
        {sep}
        <Link
          href={`/${at.owner}/environments/${at.env}`}
          className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
        >
          {/* The URL carries the ID; the NAME is what the page underneath knows. Until it says so,
              the id is the honest thing to show. */}
          {meta?.title ?? at.env}
        </Link>
      </>
    );
  }
  if (at.kind === "image") {
    return (
      <>
        <OwnerLink owner={at.owner} />
        {sep}
        <SectionLink owner={at.owner} label="Container Images" />
        {sep}
        <Link
          href={`/${at.owner}/registries/${at.image}`}
          className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
        >
          {at.image}
        </Link>
      </>
    );
  }

  return (
    <>
      <OwnerLink owner={at.owner} />
      {sep}
      <SectionLink owner={at.owner} label="Code Repos" />
      {sep}
      <Link
        href={`/${at.owner}/${at.repo}`}
        className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
      >
        {at.repo}
        {/* Only once the layout beneath has said so. A badge that guessed would
            be worse than one that arrives a moment later. */}
        {meta?.visibility && <Badge variant="outline">{meta.visibility}</Badge>}
      </Link>
    </>
  );
}
