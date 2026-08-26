import { Bone, Skeleton } from "@/components/app/skeleton";

/** accept-invite.tsx: one `max-w-md` card — heading, two lines of copy, a button. The page draws
 *  its own container. Without this it fell through to the home skeleton: two columns and a
 *  second <main>. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton className="max-w-md border border-border bg-card px-6 py-8">
        <Bone className="h-6 w-48" />
        <Bone className="mt-3 h-5 w-full" />
        <Bone className="mt-1.5 h-5 w-4/5" />
        <Bone className="mt-6 h-9 w-36" />
      </Skeleton>
    </main>
  );
}
