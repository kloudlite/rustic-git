import { Bone, ListBones, Skeleton } from "@/components/app/skeleton";

/** pulls.tsx: title with the New button at the far end, then the list. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="flex items-center gap-3">
        <Bone className="h-7 w-36" />
        <Bone className="ml-auto h-9 w-32" />
      </div>
      <ListBones rows={6} className="mt-6" />
    </Skeleton>
  );
}
