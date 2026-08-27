import { Bone, Skeleton } from "@/components/app/skeleton";

/** A pull's files, below the layout's header: `lg:grid-cols-code` — the file tree on the
 *  LEFT (260px, sticky) and the diffs on the right. The conversation skeleton puts a 320px
 *  aside on the right, so sharing it moved every diff by the width of two columns. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="mt-6 grid min-w-0 gap-8 lg:grid-cols-code">
        <aside className="hidden min-w-0 lg:block">
          <Bone className="h-4 w-20" />
          <div className="mt-2 grid gap-1.5">
            {Array.from({ length: 5 }, (_, i) => <Bone key={i} className="h-5 w-full" />)}
          </div>
        </aside>
        <div className="min-w-0">
          {[0, 1].map((i) => (
            <div key={i} className={`border border-border bg-card ${i ? "mt-6" : ""}`}>
              <div className="border-b border-border px-4 py-2.5"><Bone className="h-4 w-48" /></div>
              <div className="grid gap-1.5 px-4 py-3">
                {Array.from({ length: 6 }, (_, j) => <Bone key={j} className={j % 3 === 0 ? "h-4 w-5/6" : "h-4 w-full"} />)}
              </div>
            </div>
          ))}
        </div>
      </div>
    </Skeleton>
  );
}
