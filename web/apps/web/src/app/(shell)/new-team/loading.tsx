import { Bone, Skeleton } from "@/components/app/skeleton";

/** A single form: title, two labelled fields, a submit. The page draws its own container. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton className="max-w-2xl">
        <Bone className="h-7 w-40" />
        <Bone className="mt-2 h-3 w-72 max-w-full" />
        <div className="mt-6 grid gap-5">
          <div><Bone className="h-3 w-16" /><Bone className="mt-2 h-9 w-full max-w-md" /></div>
          <div><Bone className="h-3 w-24" /><Bone className="mt-2 h-9 w-full max-w-md" /></div>
          <Bone className="h-9 w-28" />
        </div>
      </Skeleton>
    </main>
  );
}
