import { Bone, LineBones, Skeleton, ToolbarBones } from "@/components/app/skeleton";

/** `/{owner}`: dashboard.tsx — the repo list beside the activity rail, on `grid-cols-overview`.
 *
 *  This file serves ONLY the group's index page in practice: every child route under `(org)`
 *  carries its own loading.tsx, because Next picks the nearest one and this would otherwise
 *  paint the dashboard's two columns over a list page. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <ToolbarBones />
          <LineBones rows={6} className="mt-5" />
        </section>
        <aside>
          <Bone className="h-3 w-24" />
          <LineBones rows={5} className="mt-4" />
        </aside>
      </div>
    </Skeleton>
  );
}
