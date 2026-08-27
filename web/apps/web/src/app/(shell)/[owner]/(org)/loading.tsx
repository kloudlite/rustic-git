import { Bone, LineBones, ListBones, Skeleton, ToolbarBones } from "@/components/app/skeleton";

/** `/{owner}`: team-overview.tsx — the repo list beside the activity rail, on `grid-cols-overview`.
 *
 *  This file serves ONLY the group's index page in practice: every child route under `(org)`
 *  carries its own loading.tsx, because Next picks the nearest one and this would otherwise
 *  paint the dashboard's two columns over a list page. */
export default function Loading() {
  return (
    <Skeleton>
      {/* The "View as" row, `h-8` + `mb-4` like the page. The "Running now" strip above
          the grid is conditional and is NOT drawn: a skeleton that guesses at a block
          which is usually absent moves the page more than a short one does. */}
      <div className="mb-4 flex justify-end">
        <Bone className="h-8 w-36" />
      </div>
      <div className="grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <ToolbarBones />
          <ListBones rows={4} className="mt-5" />
        </section>
        <aside>
          <Bone className="h-3 w-24" />
          <LineBones rows={5} className="mt-4" />
        </aside>
      </div>
    </Skeleton>
  );
}
