import { Bone, Skeleton } from "@/components/app/skeleton";

/** Filter box, the live list, then the collapsed Snapshots heading — the two groups the page
 *  lands as. `py-3.5` rows, not `ListBones`'s `py-4`: these rows are 75px, and a skeleton that
 *  draws a different height is worse than none, because the page then jumps twice.
 *
 *  Per route rather than one file at the `(org)` level: Next uses the NEAREST loading.tsx, so a
 *  group-level one would shadow every child. */
function Rows({ rows }: { rows: number }) {
  return (
    <div className="border border-border bg-card">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="border-b border-border px-5 py-3.5 last:border-b-0">
          <Bone className="h-6 w-56" />
          <Bone className="mt-1 h-5 w-72 max-w-full" />
        </div>
      ))}
    </div>
  );
}

export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-8 w-full max-w-xs" />
      <div className="mt-5">
        <Rows rows={3} />
      </div>
      <Bone className="mt-7 h-4 w-64" />
    </Skeleton>
  );
}
