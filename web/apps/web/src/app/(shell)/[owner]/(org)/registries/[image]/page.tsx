import { notFound, redirect } from "next/navigation";
import { Lock } from "lucide-react";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { imageTags } from "@/lib/browse";
import { size, when } from "@/lib/time";
import { BackLink } from "@/components/repo/back-link";
import { CopyLine } from "@/components/app/image-list";

/** One image: what a `docker pull` on this name can resolve to, and the
 *  commands that grow it. Laid out like the package pages people already
 *  know — install up top, versions under it, facts in a right rail. */
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

  const host = (process.env.RUSTIC_GIT_REGISTRY_HOST ?? "cr.khost.dev").replace(/\/$/, "");
  const list = tags.value;
  const lastPublished = list.reduce<number | null>((max, t) => {
    if (t.pushed_ms === null) return max;
    return max === null ? t.pushed_ms : Math.max(max, t.pushed_ms);
  }, null);
  const totalBytes = list.reduce((sum, t) => sum + t.bytes, 0);
  const totalPulls = list.reduce((sum, t) => sum + t.pulls, 0);

  return (
    <section>
      <BackLink href={`/${owner}/registries`}>Container Images</BackLink>

      <div className="mt-4 flex flex-wrap items-center gap-2.5">
        <h1 className="text-title font-semibold tracking-title">{image}</h1>
        {/* Every image is private today — there is no visibility toggle, so this
            states the fact rather than implying a choice. Same chip as the repo
            list draws, so the two pages read as one product. */}
        <span className="flex shrink-0 items-center gap-1 border border-border px-1.5 py-0.5 text-micro font-medium text-muted-foreground">
          <Lock className="size-3" />
          Private
        </span>
      </div>
      <p className="mt-1.5 text-sm2 text-muted-foreground">
        {list.length} {list.length === 1 ? "tag" : "tags"} · {size(totalBytes)}
        {lastPublished !== null && <> · updated {when(lastPublished)}</>}
      </p>

      <div className="mt-6 grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_21rem]">
        <div className="min-w-0">
          <div className="border border-border bg-card">
            <h2 className="border-b border-border bg-muted/30 px-5 py-3 text-sm2 font-medium">
              Install from the command line
            </h2>
            <div className="p-5">
              <CopyLine value={`docker pull ${host}/${owner}/${image}:latest`} />
            </div>
          </div>

          <div className="mt-6 border border-border bg-card">
            <h2 className="border-b border-border bg-muted/30 px-5 py-3 text-sm2 font-medium">
              Tagged versions
            </h2>
            {list.length === 0 ? (
              <div className="px-5 py-14 text-center">
                <p className="text-sm2 font-medium">No tags</p>
                <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
                  Every tag on this image has been removed.
                </p>
              </div>
            ) : (
              <ul className="divide-y divide-border">
                {list.map((t) => (
                  <li key={t.tag} className="px-5 py-4">
                    <div className="flex items-center gap-3">
                      <span className="truncate text-body font-medium">{t.tag}</span>
                      <span className="ml-auto flex shrink-0 items-center gap-4 text-caption text-muted-foreground">
                        <span className="tabular-nums">{t.pulls} {t.pulls === 1 ? "pull" : "pulls"}</span>
                        <span>{t.pushed_ms === null ? "published unknown" : `published ${when(t.pushed_ms)}`}</span>
                        <span className="w-16 text-right tabular-nums">{size(t.bytes)}</span>
                      </span>
                    </div>
                    <div className="mt-2 max-w-xl">
                      <CopyLine value={t.digest} compact />
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>

        <aside className="min-w-0 space-y-6">
          <div className="border border-border bg-card">
            <h2 className="border-b border-border bg-muted/30 px-5 py-3 text-sm2 font-medium">Details</h2>
            <dl className="divide-y divide-border text-sm2">
              {[
                ["Owner", owner],
                ["Last published", lastPublished === null ? "Unknown" : when(lastPublished)],
                ["Tags", String(list.length)],
                ["Total size", size(totalBytes)],
                ["Pulls", String(totalPulls)],
              ].map(([dt, dd]) => (
                <div key={dt} className="flex items-baseline justify-between gap-4 px-5 py-2.5">
                  <dt className="text-muted-foreground">{dt}</dt>
                  <dd className="truncate font-medium">{dd}</dd>
                </div>
              ))}
            </dl>
          </div>

          {/* No upload UI exists, so this is the closest thing to it: the three
              lines that put a new tag here. */}
          <div className="border border-border bg-card">
            <h2 className="border-b border-border bg-muted/30 px-5 py-3 text-sm2 font-medium">Push a new tag</h2>
            <div className="space-y-2 p-5">
              <CopyLine value={`docker login ${host} -u ${owner}`} />
              <CopyLine value={`docker tag ${image} ${host}/${owner}/${image}:latest`} />
              <CopyLine value={`docker push ${host}/${owner}/${image}:latest`} />
            </div>
          </div>
        </aside>
      </div>
    </section>
  );
}
