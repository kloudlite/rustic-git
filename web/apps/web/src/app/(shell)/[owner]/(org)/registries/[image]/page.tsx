import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { imageTags, shortOid } from "@/lib/browse";
import { size, when } from "@/lib/time";
import { BackLink } from "@/components/repo/back-link";
import { Badge } from "@/components/ui/badge";
import { CopyLine } from "@/components/app/image-list";

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

  const host = (process.env.RUSTIC_GIT_CLONE_HOST ?? "cr.khost.dev").replace(/\/$/, "");
  const list = tags.value;
  const latest = list.find((t) => t.tag === "latest");
  const lastPublished = list.reduce<number | null>((max, t) => {
    if (t.pushed_ms === null) return max;
    return max === null ? t.pushed_ms : Math.max(max, t.pushed_ms);
  }, null);
  const totalBytes = list.reduce((sum, t) => sum + t.bytes, 0);

  return (
    <section className="mx-auto max-w-4xl">
      <BackLink href={`/${owner}/registries`}>Container Images</BackLink>

      <div className="mt-3 flex flex-wrap items-center gap-3">
        <h1 className="text-title font-semibold tracking-title">{image}</h1>
        <Badge variant="outline">Private</Badge>
        {/* Every image is private today — there is no visibility toggle, so this
            states the fact rather than implying a choice. */}
        {latest && (
          <span
            className="truncate font-mono text-caption text-muted-foreground"
            title={latest.digest}
          >
            {shortOid(latest.digest.replace(/^sha256:/, ""))}
          </span>
        )}
      </div>

      <div className="mt-6 grid grid-cols-1 gap-6 lg:grid-cols-[1fr_18rem]">
        <div className="min-w-0">
          <div className="border border-border bg-card p-5">
            <h2 className="text-sm2 font-medium">Install from the command line</h2>
            <div className="mt-3">
              <CopyLine value={`docker pull ${host}/${owner}/${image}:latest`} />
            </div>
          </div>

          <h2 className="mt-6 text-sm2 font-medium">Recent tagged versions</h2>
          {list.length === 0 ? (
            <div className="mt-3 border border-border bg-card px-5 py-14 text-center">
              <p className="text-sm2 font-medium">No tags</p>
              <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
                Every tag on this image has been removed.
              </p>
            </div>
          ) : (
            <ul className="mt-3 divide-y divide-border border border-border bg-card">
              {list.map((t) => (
                <li key={t.tag} className="flex flex-wrap items-center gap-3 px-5 py-3">
                  <span className="inline-flex items-center rounded-4xl border border-border bg-muted/50 px-2.5 py-0.5 font-mono text-caption font-medium">
                    {t.tag}
                  </span>
                  <span className="font-mono text-caption text-muted-foreground" title={t.digest}>
                    {shortOid(t.digest.replace(/^sha256:/, ""))}
                  </span>
                  <span className="text-caption text-muted-foreground">
                    {t.pushed_ms === null ? "Published unknown" : `Published ${when(t.pushed_ms)}`}
                  </span>
                  <span className="ml-auto text-caption text-muted-foreground">{size(t.bytes)}</span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <aside className="lg:pt-[3.75rem]">
          <div className="border border-border bg-card p-5">
            <h2 className="text-sm2 font-medium">Details</h2>
            <dl className="mt-3 space-y-3 text-sm2">
              <div>
                <dt className="text-caption text-muted-foreground">Owner</dt>
                <dd className="mt-0.5">{owner}</dd>
              </div>
              <div>
                <dt className="text-caption text-muted-foreground">Last published</dt>
                <dd className="mt-0.5">{lastPublished === null ? "Unknown" : when(lastPublished)}</dd>
              </div>
              <div>
                <dt className="text-caption text-muted-foreground">Tags</dt>
                <dd className="mt-0.5">{list.length}</dd>
              </div>
              <div>
                <dt className="text-caption text-muted-foreground">Total size</dt>
                <dd className="mt-0.5">{size(totalBytes)}</dd>
              </div>
            </dl>
          </div>
        </aside>
      </div>
    </section>
  );
}
