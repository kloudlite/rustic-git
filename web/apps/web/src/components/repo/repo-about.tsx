import Link from "next/link";
import { GitBranch, History, Scale, Tag } from "lucide-react";
import { COMMITS, REPO } from "@/lib/mock-repo";

function Fact({ icon: Icon, href, children }: { icon: typeof Tag; href?: string; children: React.ReactNode }) {
  const cls = "flex h-7 items-center gap-2 text-sm2 text-muted-foreground";
  const body = <><Icon className="size-3.5" />{children}</>;
  return href
    ? <Link href={href} className={`${cls} transition-colors hover:text-foreground`}>{body}</Link>
    : <span className={cls}>{body}</span>;
}

/** What the repo is, in a rail: the description and four facts. */
export function RepoAbout({ base }: { base: string }) {
  return (
    <div>
      <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">About</h2>
      <p className="mt-2 text-sm2 leading-relaxed text-foreground/90">{REPO.description}</p>
      <div className="mt-3 grid">
        <Fact icon={History} href={`${base}/commits`}>{COMMITS.length} commits</Fact>
        <Fact icon={GitBranch}>{REPO.branches.length} branches</Fact>
        <Fact icon={Tag}>{REPO.tags.length} tags</Fact>
        <Fact icon={Scale}>SSPL-1.0 license</Fact>
      </div>
    </div>
  );
}
