import { Bone, Skeleton } from "@/components/app/skeleton";

/** commits.tsx: the ref picker, then commits grouped under an 18px day heading on `mt-6`
 *  (lands at 180), each list at `mt-3` (210) with 71px `px-5 py-3.5` rows. */
export function CommitRows({ rows }: { rows: number }) {
  return (
    <div className="border border-border bg-card">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="border-b border-border px-5 py-3.5 last:border-b-0">
          <Bone className="h-5 w-80 max-w-full" />
          <Bone className="mt-[3px] h-5 w-56" />
        </div>
      ))}
    </div>
  );
}

export default function Loading() {
  return (
    <Skeleton>
      <div className="flex items-center gap-3">
        <Bone className="h-8 w-36" />
        <Bone className="h-4 w-24" />
      </div>
      <div className="mt-6 grid gap-8">
        {[3, 2].map((n, i) => (
          <div key={i}>
            <Bone className="h-[18px] w-28" />
            <div className="mt-3"><CommitRows rows={n} /></div>
          </div>
        ))}
      </div>
    </Skeleton>
  );
}
