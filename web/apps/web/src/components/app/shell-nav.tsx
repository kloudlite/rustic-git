"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { NavTabs, type NavTab } from "@/components/app/nav-tabs";
import { useRepoMeta } from "@/components/app/shell-context";
import { Badge } from "@/components/ui/badge";

/** A repo tab, as the shell is given it: the icon is already rendered, because a
 *  component cannot cross from the server into here, and the href is a suffix
 *  because which repo it belongs to is only known from the URL. */
export type RepoTabSpec = { suffix: string; label: string; icon: React.ReactNode; end?: boolean };

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
 *  tells an image page apart from the list page it hangs off of. */
function place(pathname: string, reserved: readonly string[]) {
  const parts = pathname.split("/").filter(Boolean);
  if (parts.length >= 3 && parts[1] === "registries") {
    return { kind: "image" as const, owner: parts[0], image: parts[2] };
  }
  if (parts.length >= 2 && !reserved.includes(parts[1])) {
    return { kind: "repo" as const, owner: parts[0], repo: parts[1] };
  }
  return { kind: "org" as const };
}

export function ShellTabs({
  orgTabs,
  repoTabs,
  imageTabs,
  reserved,
  className,
}: {
  orgTabs: NavTab[];
  repoTabs: RepoTabSpec[];
  imageTabs: RepoTabSpec[];
  reserved: readonly string[];
  className?: string;
}) {
  const at = place(usePathname(), reserved);
  if (at.kind === "org") {
    return <NavTabs tabs={orgTabs} className={className} aria-label="Sections" />;
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
  const base = `/${at.owner}/${at.repo}`;
  return (
    <NavTabs
      tabs={repoTabs.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end }))}
      back={{ href: `/${at.owner}`, label: "Repos" }}
      className={className}
      aria-label={at.repo}
    />
  );
}

/** The breadcrumb, which grows a segment inside a repo or an image. */
export function ShellCrumb({
  section,
  imageSection,
  reserved,
  switcher,
}: {
  /** The Code Repos entry, already rendered — the list a repo came from. */
  section: React.ReactNode;
  /** The Container Images entry, already rendered — the list an image came from. */
  imageSection: React.ReactNode;
  reserved: readonly string[];
  /** The team switcher, shown when the crumb is not inside a repo or an image. */
  switcher: React.ReactNode;
}) {
  const at = place(usePathname(), reserved);
  const meta = useRepoMeta();
  if (at.kind === "org") return <>{switcher}</>;

  if (at.kind === "image") {
    return (
      <>
        <Link
          href={`/${at.owner}`}
          className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
        >
          <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
          {at.owner}
        </Link>
        <span className="text-muted-foreground/40" aria-hidden>/</span>
        {imageSection}
        <span className="text-muted-foreground/40" aria-hidden>/</span>
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
      <Link
        href={`/${at.owner}`}
        className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
      >
        <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
        {at.owner}
      </Link>
      <span className="text-muted-foreground/40" aria-hidden>/</span>
      {section}
      <span className="text-muted-foreground/40" aria-hidden>/</span>
      <Link
        href={`/${at.owner}/${at.repo}`}
        className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
      >
        {at.repo}
        {/* Only once the layout beneath has said so. A badge that guessed would
            be worse than one that arrives a moment later. */}
        {meta && <Badge variant="outline">{meta.visibility}</Badge>}
      </Link>
    </>
  );
}
