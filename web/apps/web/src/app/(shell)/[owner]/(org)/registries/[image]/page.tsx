import { Lock } from "lucide-react";
import { guardImage } from "./guard";
import { size, when } from "@/lib/time";
import { CopyLine } from "@/components/app/image-list";
import { registryHost } from "@/lib/clone";

/** One image's Details tab: what a `docker pull` on this name can resolve to, the
 *  facts about it, and the commands that grow it. The tag list itself lives on the
 *  Tags tab now — this page is the summary a repo's Code tab would be. */
export default async function ImagePage({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags: list } = await guardImage(owner, image);

  const host = await registryHost();
  const lastPublished = list.reduce<number | null>((max, t) => {
    if (t.pushed_ms === null) return max;
    return max === null ? t.pushed_ms : Math.max(max, t.pushed_ms);
  }, null);
  const totalBytes = list.reduce((sum, t) => sum + t.bytes, 0);
  const totalPulls = list.reduce((sum, t) => sum + t.pulls, 0);

  return (
    <section>
      <div className="flex flex-wrap items-center gap-2.5">
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
