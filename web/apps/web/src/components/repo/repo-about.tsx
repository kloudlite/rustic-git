import Link from "next/link";
import { GitBranch, History, Lock, Tag } from "lucide-react";

function Fact({ icon: Icon, href, children }: { icon: typeof Tag; href?: string; children: React.ReactNode }) {
  const cls = "flex h-6 items-center gap-2 text-caption text-muted-foreground";
  const body = <><Icon className="size-3.5" />{children}</>;
  return href
    ? <Link href={href} className={`${cls} transition-colors hover:text-foreground`}>{body}</Link>
    : <span className={cls}>{body}</span>;
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** What the repo is, in a rail: the description it was created with, and the
 *  counts that come free with the refs the page already fetched.
 *
 *  No languages and no contributors block: both need a walk of the whole tree or
 *  the whole history, and neither is served. A rail of plausible-looking numbers
 *  nobody computed is worse than a shorter rail. */
export function RepoAbout({
  base,
  description,
  branches,
  tags,
  isPrivate,
}: {
  base: string;
  description: string;
  branches: number;
  tags: number;
  isPrivate: boolean;
}) {
  return (
    <div className="grid gap-7">
      <section>
        <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">About</h2>
        {description ? (
          <p className="mt-2 text-sm2 leading-relaxed text-foreground/90">{description}</p>
        ) : (
          <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">No description.</p>
        )}
        <div className="mt-3 grid">
          <Fact icon={History} href={`${base}/commits`}>History</Fact>
          <Fact icon={GitBranch}>{plural(branches, "branch").replace("branchs", "branches")}</Fact>
          <Fact icon={Tag}>{plural(tags, "tag")}</Fact>
          {isPrivate && <Fact icon={Lock}>Private</Fact>}
        </div>
      </section>
    </div>
  );
}
