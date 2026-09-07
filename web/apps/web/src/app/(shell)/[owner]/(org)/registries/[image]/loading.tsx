import { Bone, LineBones, Skeleton } from "@/components/app/skeleton";

/** An image: title with a visibility chip, blurb, then the manifest list beside a 21rem aside
 *  — the same `lg:grid-cols-[minmax(0,1fr)_21rem]` the page uses. Tags and settings beneath it
 *  carry their own: no blurb on either, and their first block lands 26px and 18px higher. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="flex h-[30px] items-center gap-2.5">
        <Bone className="h-[30px] w-48" />
        <Bone className="h-[23px] w-16" />
      </div>
      <Bone className="mt-1.5 h-5 w-80 max-w-full" />
      <div className="mt-6 grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_21rem]">
        <div className="border border-border bg-card">
          <div className="border-b border-border bg-muted/30 px-5 py-3"><Bone className="h-4 w-20" /></div>
          <LineBones rows={5} className="border-0" />
        </div>
        <div className="border border-border bg-card p-5">
          <Bone className="h-4 w-24" />
          <Bone className="mt-3 h-9 w-full" />
          <Bone className="mt-2 h-9 w-full" />
        </div>
      </div>
    </Skeleton>
  );
}
