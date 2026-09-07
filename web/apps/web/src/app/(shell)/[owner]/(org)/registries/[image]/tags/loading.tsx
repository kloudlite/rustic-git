import { Bone, Skeleton } from "@/components/app/skeleton";

/** Tags: a 30px title, then one card on `mt-6` (lands at 186) — a 45px header strip and 89px
 *  rows, each a tag line over a copy line. */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-[30px] w-48" />
      <div className="mt-6 border border-border bg-card">
        <div className="border-b border-border bg-muted/30 px-5 py-3"><Bone className="h-5 w-20" /></div>
        {Array.from({ length: 3 }, (_, i) => (
          <div key={i} className="border-b border-border px-5 py-4 last:border-b-0">
            <Bone className="h-[21px] w-32" />
            <Bone className="mt-2 h-7 w-[576px] max-w-full" />
          </div>
        ))}
      </div>
    </Skeleton>
  );
}
