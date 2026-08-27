import { Bone, Skeleton } from "@/components/app/skeleton";

/** new-pull-form.tsx: title, the base→compare strip, title field, body, submit. */
export default function Loading() {
  return (
    <Skeleton className="max-w-2xl">
      <Bone className="h-[30px] w-48" />
      <div className="mt-6 grid gap-5">
        <div className="flex h-32 items-end gap-3 border border-border bg-card p-4">
          <div><Bone className="h-4 w-12" /><Bone className="mt-2 h-9 w-40" /></div>
          <Bone className="mb-2.5 size-4" />
          <div><Bone className="h-4 w-16" /><Bone className="mt-2 h-9 w-40" /></div>
        </div>
        <Bone className="h-9 w-full" />
        <Bone className="h-32 w-full" />
        <Bone className="h-9 w-40" />
      </div>
    </Skeleton>
  );
}
