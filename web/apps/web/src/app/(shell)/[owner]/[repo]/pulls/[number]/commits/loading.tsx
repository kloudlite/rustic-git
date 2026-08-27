import { Bone, Skeleton } from "@/components/app/skeleton";
import { CommitRows } from "@/app/(shell)/[owner]/[repo]/commits/loading";

/** A pull's commits: the pull header, then a 20px day heading on `mt-6` (305) and the commit
 *  list on `mt-2` (333). The conversation skeleton's aside grid does not belong here. */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-7 w-24" />
      <div className="mt-3">
        <Bone className="h-[25px] w-96 max-w-full" />
        <div className="mt-2.5 flex items-center gap-2">
          <Bone className="h-6 w-20" />
          <Bone className="h-5 w-64 max-w-full" />
        </div>
        <div className="mt-[22px] flex h-[37px] items-center gap-6 border-b border-border">
          <Bone className="h-4 w-24" />
          <Bone className="h-4 w-12" />
          <Bone className="h-4 w-16" />
        </div>
      </div>
      <Bone className="mt-6 h-5 w-28" />
      <div className="mt-2"><CommitRows rows={2} /></div>
    </Skeleton>
  );
}
