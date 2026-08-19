import Link from "next/link";
import { GitBranch, History, Scale, Tag } from "lucide-react";
import { FileTree } from "@/components/repo/file-tree";
import { RefPicker } from "@/components/repo/ref-picker";
import { COMMITS, REPO } from "@/lib/mock-repo";

function Fact({ icon: Icon, href, children }: { icon: typeof Tag; href?: string; children: React.ReactNode }) {
  const cls = "flex h-7 items-center gap-2 text-sm2 text-muted-foreground";
  const body = <><Icon className="size-3.5" />{children}</>;
  return href
    ? <Link href={href} className={`${cls} transition-colors hover:text-foreground`}>{body}</Link>
    : <span className={cls}>{body}</span>;
}

/** The left column of every Code page: which ref, how to reach a file, the tree,
 *  and the repo's facts. Everything here is about *the repo at this ref*; the
 *  right column is about the path you are looking at. */
export function RepoSidebar({ base, openDir, activePath }: { base: string; openDir?: string; activePath?: string }) {
  return (
    <div className="grid gap-5">
      <RefPicker current={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} className="w-full justify-between" />
      <FileTree base={base} openDir={openDir} activePath={activePath} />
      <div className="border-t border-border pt-4">
        <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">About</h2>
        <p className="mt-2 text-sm2 leading-relaxed text-foreground/90">{REPO.description}</p>
        <div className="mt-3 grid">
          <Fact icon={History} href={`${base}/commits`}>{COMMITS.length} commits</Fact>
          <Fact icon={GitBranch}>{REPO.branches.length} branches</Fact>
          <Fact icon={Tag}>{REPO.tags.length} tags</Fact>
          <Fact icon={Scale}>SSPL-1.0 license</Fact>
        </div>
      </div>
    </div>
  );
}
