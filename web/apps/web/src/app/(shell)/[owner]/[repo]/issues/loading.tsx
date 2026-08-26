import { Bone, Skeleton } from "@/components/app/skeleton";

/** `NotYet`: a title, then one centred placeholder card (`px-5 py-14`). */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-[30px] w-40" />
      <div className="mt-6 border border-border bg-card px-5 py-14"><Bone className="mx-auto h-5 w-96 max-w-full" /></div>
    </Skeleton>
  );
}
