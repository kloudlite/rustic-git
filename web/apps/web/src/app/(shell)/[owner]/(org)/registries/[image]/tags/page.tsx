import { guardImage } from "../guard";
import { size, when } from "@/lib/time";
import { CopyLine } from "@/components/app/image-list";

/** The Tags tab: every tag this image has, full width — the list that used to
 *  crowd the Details tab. Same data fetch, same empty state, just its own page
 *  now that the tab row carries the navigation. */
export default async function ImageTagsPage({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags: list } = await guardImage(owner, image);

  return (
    <section>
      <h1 className="text-title font-semibold tracking-title">Tags</h1>

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
    </section>
  );
}
