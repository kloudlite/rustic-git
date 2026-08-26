import { Bone, LineBones, ListBones, Skeleton, ToolbarBones } from "@/components/app/skeleton";

/** `/{owner}`: dashboard.tsx — the repo list beside the activity rail, on `grid-cols-overview`. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <ToolbarBones />
          <ListBones rows={6} className="mt-5" />
        </section>
        <aside>
          <Bone className="h-3 w-24" />
          <LineBones rows={5} className="mt-4" />
        </aside>
      </div>
    </Skeleton>
  );
}
