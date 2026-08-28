import { Bone, Skeleton } from "@/components/app/skeleton";

/** BODY ONLY — the header and tabs come from `[id]/layout.tsx`, which is already painted.
 *  The take-snapshot row, then the rail: a dot-width column and two text lines per node, the
 *  shape `env-snapshots.tsx` lands as. */
export default function Loading() {
  return (
    <Skeleton className="mt-5">
      <div className="flex items-center gap-2">
        <Bone className="h-8 w-full max-w-sm" />
        <Bone className="h-8 w-32" />
      </div>
      <div className="mt-5 border border-border bg-card">
        {Array.from({ length: 4 }, (_, i) => (
          <div key={i} className="flex items-start gap-3.5 border-b border-border px-5 py-3.5 last:border-b-0">
            <Bone className="mt-1.5 size-2.5 shrink-0 rounded-full" />
            <div className="min-w-0 flex-1">
              <Bone className="h-5 w-56 max-w-full" />
              <Bone className="mt-1 h-4 w-40" />
            </div>
            <Bone className="h-7 w-20" />
          </div>
        ))}
      </div>
    </Skeleton>
  );
}
