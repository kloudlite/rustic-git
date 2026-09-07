import { Bone, Skeleton } from "@/components/app/skeleton";
import { CommitRows } from "@/app/(shell)/[owner]/[repo]/commits/loading";

/** A pull's commits, below the layout's header: a 20px day heading on `mt-6` and the
 *  commit list on `mt-2`. The conversation skeleton's aside grid does not belong here. */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="mt-6 h-5 w-28" />
      <div className="mt-2"><CommitRows rows={2} /></div>
    </Skeleton>
  );
}
