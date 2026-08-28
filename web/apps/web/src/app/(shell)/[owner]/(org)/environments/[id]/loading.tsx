import { Bone, Skeleton } from "@/components/app/skeleton";

/** BODY ONLY. The back link, the header and the tab row live in `layout.tsx`, which is already
 *  painted when this shows — drawing them again would stack two headers for one page. */
export default function Loading() {
  return (
    <Skeleton className="mt-5">
      <div className="border border-border bg-card">
        {Array.from({ length: 3 }, (_, i) => (
          <div key={i} className="flex items-center gap-4 border-b border-border px-5 py-3.5 last:border-b-0">
            <div className="min-w-0 flex-1">
              <Bone className="h-5 w-32" />
              <Bone className="mt-1 h-5 w-64 max-w-full" />
            </div>
            <Bone className="h-5 w-40" />
          </div>
        ))}
      </div>
    </Skeleton>
  );
}
