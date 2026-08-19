import Link from "next/link";
import { GitBranch, History, Scale, Tag } from "lucide-react";
import { COMMITS, CONTRIBUTORS, LANGUAGES, REPO } from "@/lib/mock-repo";

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

/** What the repo is, in a rail: description and facts, what it is written in, and
 *  who writes it. Each block is small on purpose — the rail is a glance, and each
 *  heading links to the page that goes deeper. */
export function RepoAbout({ base }: { base: string }) {
  return (
    <div className="grid gap-7">
      <section>
        <Heading>About</Heading>
        <p className="mt-2 text-sm2 leading-relaxed text-foreground/90">{REPO.description}</p>
        <div className="mt-3 grid">
          <Fact icon={History} href={`${base}/commits`}>{COMMITS.length} commits</Fact>
          <Fact icon={GitBranch}>{REPO.branches.length} branches</Fact>
          <Fact icon={Tag}>{REPO.tags.length} tags</Fact>
          <Fact icon={Scale}>SSPL-1.0 license</Fact>
        </div>
      </section>

      <section>
        <Heading>Languages</Heading>
        <div className="mt-2.5 flex h-2 w-full gap-px overflow-hidden" role="img" aria-label={LANGUAGES.map((l) => `${l.name} ${l.pct}%`).join(", ")}>
          {LANGUAGES.map((l) => (
            <span key={l.name} style={{ width: `${l.pct}%`, background: l.color }} />
          ))}
        </div>
        <ul className="mt-2.5 grid grid-cols-2 gap-x-4 gap-y-1">
          {LANGUAGES.map((l) => (
            <li key={l.name} className="flex items-center gap-2 text-caption">
              <span className="size-2 shrink-0" style={{ background: l.color }} aria-hidden />
              <span className="font-medium">{l.name}</span>
              <span className="text-muted-foreground">{l.pct}%</span>
            </li>
          ))}
        </ul>
      </section>

      <section>
        <Heading
          aside={
            <Link href={`${base}/commits`} className="text-caption text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline">
              {CONTRIBUTORS.length}
            </Link>
          }
        >
          Contributors
        </Heading>
        <ul className="mt-2.5 flex flex-wrap gap-1.5">
          {CONTRIBUTORS.map((c) => (
            <li key={c.login}>
              <Link
                href={`/${c.login}`}
                title={`${c.name} · ${c.commits} commits`}
                className="flex size-7 items-center justify-center bg-muted text-micro font-semibold text-muted-foreground transition-colors hover:bg-primary hover:text-primary-foreground"
              >
                {c.name.split(" ").map((p) => p[0]).slice(0, 2).join("")}
              </Link>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}
