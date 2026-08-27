import { Bone, Skeleton } from "@/components/app/skeleton";

/** diff.tsx: back link, the commit card (message, meta strip), then file diffs. */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-7 w-20" />
      <div className="mt-3 border border-border bg-card">
        <div className="px-5 py-4">
          <Bone className="h-5 w-96 max-w-full" />
          <Bone className="mt-2 h-3 w-72 max-w-full" />
        </div>
        <div className="flex items-center gap-4 border-t border-border bg-muted/40 px-5 py-2.5">
          <Bone className="h-3 w-24" />
          <Bone className="h-3 w-32" />
          <Bone className="ml-auto h-3 w-20" />
        </div>
      </div>
      {/* "N files changed" — 20px on mt-6, then the first diff on mt-3. */}
      <Bone className="mt-6 h-5 w-40" />
      {[0, 1].map((i) => (
        <div key={i} className="mt-3 border border-border bg-card">
          <div className="border-b border-border px-4 py-2.5"><Bone className="h-3 w-48" /></div>
          <div className="grid gap-1.5 px-4 py-3">
            {Array.from({ length: 6 }, (_, j) => <Bone key={j} className={j % 3 === 0 ? "h-3 w-5/6" : "h-3 w-full"} />)}
          </div>
        </div>
      ))}
    </Skeleton>
  );
}
