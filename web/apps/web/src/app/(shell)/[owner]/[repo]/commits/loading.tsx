import { Bone, ListBones, Skeleton } from "@/components/app/skeleton";

/** commits.tsx: ref picker, then commits grouped under a day heading. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="flex items-center gap-3">
        <Bone className="h-8 w-36" />
        <Bone className="h-4 w-24" />
      </div>
      <div className="mt-6 grid gap-8">
        {[4, 3].map((n, i) => (
          <div key={i}>
            <Bone className="h-3 w-28" />
            <ListBones rows={n} className="mt-3" />
          </div>
        ))}
      </div>
    </Skeleton>
  );
}
