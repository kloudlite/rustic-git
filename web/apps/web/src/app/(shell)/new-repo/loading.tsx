import { Bone, Skeleton, TitleBones } from "@/components/app/skeleton";

/** A single form: title, two labelled fields, a submit. The page draws its own container. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton className="max-w-xl">
        <TitleBones width="w-48" />
        <div className="mt-8 grid gap-5">
          <div><Bone className="h-4 w-16" /><Bone className="mt-2 h-10 w-full" /></div>
          <div><Bone className="h-4 w-24" /><Bone className="mt-2 h-10 w-full" /></div>
          <Bone className="h-10 w-32" />
        </div>
      </Skeleton>
    </main>
  );
}
