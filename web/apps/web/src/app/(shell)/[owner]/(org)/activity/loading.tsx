import { Bone, LineBones, Skeleton } from "@/components/app/skeleton";

/** The activity page is a narrow column: back link, title, blurb, then the feed. */
export default function Loading() {
  return (
    <Skeleton className="mx-auto max-w-2xl">
      <Bone className="h-4 w-24" />
      <Bone className="mt-3 h-7 w-40" />
      <Bone className="mt-2 h-3 w-72 max-w-full" />
      <LineBones rows={10} className="mt-4" />
    </Skeleton>
  );
}
