import { notFound, redirect } from "next/navigation";
import { FileEditor } from "@/components/repo/file-editor";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { blob, decodeBlob, refs, resolveRef, shortRef } from "@/lib/browse";
import { pathHref } from "@/lib/utils";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string; path: string[] }>;
  searchParams: Promise<{ ref?: string }>;
}) {
  const { owner, repo, path } = await params;
  const { token } = await guardRepo(owner, repo);
  const { ref } = await searchParams;
  const file = path.join("/");

  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);
  const head = resolveRef(all.value, ref);
  if (!head) throw new Error("this repo has no branches");
  const branch = shortRef(head.name);

  // Editing is editing a BRANCH. A tag or a bare commit has nothing to move, so
  // there is nowhere for the edit to land.
  if (head.kind !== "branch") redirect(`/${owner}/${repo}/blob/${pathHref(file)}?ref=${encodeURIComponent(branch)}`);

  const b = await blob(token, owner, repo, head.oid, file);
  if (!b.ok) notFound();
  const decoded = decodeBlob(b.value);
  // Binary is not text, and a textarea would turn it into mojibake and commit
  // that. Say so where they clicked rather than opening an editor that corrupts.
  if (decoded.binary || b.value.truncated) {
    redirect(`/${owner}/${repo}/blob/${pathHref(file)}?ref=${encodeURIComponent(branch)}`);
  }

  return (
    <FileEditor
      owner={owner}
      repo={repo}
      path={file}
      branch={branch}
      // The tip the editor is reading. Sent back with the commit so a push that
      // lands mid-edit is a conflict rather than silently overwritten work.
      expect={head.oid}
      initial={decoded.text}
    />
  );
}
