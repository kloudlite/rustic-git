import { Bone, ListBones, Skeleton } from "@/components/app/skeleton";

/** pulls.tsx: title with the New button at the far end, then the list. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="flex h-8 items-center gap-3">
        <Bone className="h-[30px] w-32" />
        <Bone className="ml-auto h-8 w-40" />
      </div>
      <ListBones rows={3} className="mt-6" />
    </Skeleton>
  );
}
