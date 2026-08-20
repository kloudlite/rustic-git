import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { imageTags, shortOid } from "@/lib/browse";
import { size } from "@/lib/time";
import { BackLink } from "@/components/repo/back-link";

/** The tags of one image: what a `docker pull` on this name can resolve to. */
export default async function ImagePage({
  params,
}: {
  params: Promise<{ owner: string; image: string }>;
}) {
  const { owner, image } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const tags = await imageTags(token, owner, image);
  if (!tags.ok) {
    if (tags.kind === "unauthorized") redirect("/login?from=expired");
    if (tags.kind === "notFound") notFound();
    throw new Error(tags.message);
  }

  return (
    <section className="mx-auto max-w-2xl">
      <BackLink href={`/${owner}/registries`}>Container Images</BackLink>
      <h1 className="mt-3 text-title font-semibold tracking-title">{image}</h1>
      <p className="mt-2 text-sm2 text-muted-foreground">
        Tags pushed to {owner}/{image}.
      </p>

      {tags.value.length === 0 ? (
        <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
          <p className="text-sm2 font-medium">No tags</p>
          <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
            Every tag on this image has been removed.
          </p>
        </div>
      ) : (
        <table className="mt-5 w-full border border-border bg-card text-sm2">
          <thead>
            <tr className="border-b border-border text-left text-caption text-muted-foreground">
              <th className="px-4 py-2.5 font-medium">Tag</th>
              <th className="px-4 py-2.5 font-medium">Digest</th>
              <th className="px-4 py-2.5 font-medium">Size</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {tags.value.map((t) => (
              <tr key={t.tag}>
                <td className="px-4 py-3 font-mono">{t.tag}</td>
                <td className="px-4 py-3 font-mono text-muted-foreground" title={t.digest}>
                  {shortOid(t.digest.replace(/^sha256:/, ""))}
                </td>
                <td className="px-4 py-3 text-muted-foreground">{size(t.size)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
