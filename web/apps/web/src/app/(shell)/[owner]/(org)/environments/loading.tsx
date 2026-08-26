import { LineBones, Skeleton, ToolbarBones } from "@/components/app/skeleton";

/** A filterable list: the search/tabs/button toolbar, then the bordered list. No heading — the
 *  tab row above is the heading. Per route rather than one file at the `(org)` level, because
 *  Next uses the NEAREST loading.tsx and a group-level one would shadow this for every child. */
export default function Loading() {
  return (
    <Skeleton>
      <ToolbarBones />
      <LineBones rows={4} className="mt-5" />
    </Skeleton>
  );
}
