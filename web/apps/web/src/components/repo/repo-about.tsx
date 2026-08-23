import Link from "next/link";
import { GitBranch, History, Lock, Tag } from "lucide-react";
import { Initials } from "@/components/app/initials";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { LanguageShare } from "@/lib/languages";
import type { Contributor } from "@/lib/repo-rail";

function Fact({ icon: Icon, href, children }: { icon: typeof Tag; href?: string; children: React.ReactNode }) {
  const cls = "flex h-6 items-center gap-2 text-caption text-muted-foreground";
  const body = <><Icon className="size-3.5" />{children}</>;
  return href
    ? <Link href={href} className={`${cls} transition-colors hover:text-foreground`}>{body}</Link>
    : <span className={cls}>{body}</span>;
}

function Heading({ children, aside }: { children: React.ReactNode; aside?: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between">
      <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">{children}</h2>
      {aside}
    </div>
  );
}


/** What the repo is, in a rail: what it says it is, what it is written in, and who
 *  writes it. Each block is small on purpose — the rail is a glance.
 *
 *  Languages and contributors are both DERIVED, and each says so by what it
 *  claims: the bar is byte counts by file extension, and the faces are the
 *  authors of the commits this page already fetched, not of all history. */
export function RepoAbout({
  base,
  description,
  branches,
  tags,
  isPrivate,
  languages,
  contributors,
}: {
  base: string;
  description: string;
  branches: number;
  tags: number;
  isPrivate: boolean;
  languages: LanguageShare[];
  contributors: Contributor[];
}) {
  return (
    <div className="grid gap-7">
      <section>
        <Heading>About</Heading>
        <p className={`mt-2 text-sm2 leading-relaxed ${description ? "text-foreground/90" : "text-muted-foreground"}`}>
          {description || "No description."}
        </p>
        <div className="mt-3 grid">
          <Fact icon={History} href={`${base}/commits`}>History</Fact>
          <Fact icon={GitBranch}>{branches === 1 ? "1 branch" : `${branches} branches`}</Fact>
          <Fact icon={Tag}>{tags === 1 ? "1 tag" : `${tags} tags`}</Fact>
          {isPrivate && <Fact icon={Lock}>Private</Fact>}
        </div>
      </section>

      {languages.length > 0 && (
        <section>
          <Heading>Languages</Heading>
          <div
            className="mt-2.5 flex h-2 w-full gap-px overflow-hidden"
            role="img"
            aria-label={languages.map((l) => `${l.name} ${l.pct}%`).join(", ")}
          >
            {languages.map((l) => (
              <span key={l.name} style={{ width: `${l.pct}%`, background: l.color }} />
            ))}
          </div>
          <ul className="mt-2.5 grid grid-cols-2 gap-x-4 gap-y-1">
            {languages.map((l) => (
              <li key={l.name} className="flex items-center gap-2 text-caption">
                <span className="size-2 shrink-0" style={{ background: l.color }} aria-hidden />
                <span className="truncate font-medium">{l.name}</span>
                <span className="text-muted-foreground">{l.pct}%</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {contributors.length > 0 && (
        <section>
          <Heading
            aside={
              <Link href={`${base}/commits`} className="text-caption text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline">
                {contributors.length}
              </Link>
            }
          >
            Contributors
          </Heading>
          <ul className="mt-2.5 flex flex-wrap gap-1.5">
            {contributors.map((c) => (
              <li key={c.name}>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button type="button" className="block" aria-label={c.name}>
                      <Initials name={c.name} size={7} />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent>
                    {c.name} · {c.commits === 1 ? "1 commit" : `${c.commits} commits`} recently
                  </TooltipContent>
                </Tooltip>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
