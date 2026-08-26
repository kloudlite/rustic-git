import { ListBones, Skeleton, ToolbarBones } from "@/components/app/skeleton";

/** The owner's list pages — registries, workspaces, environments, snapshots — each open with
 *  the search/tabs toolbar and then a bordered list. No heading row: the tabs above are the
 *  heading, and the old skeleton's avatar-and-title strip was a row nothing ever replaced.
 *  The (org) layout draws the container. */
export default function Loading() {
  return (
    <Skeleton>
      <ToolbarBones />
      <ListBones rows={6} className="mt-5" />
    </Skeleton>
  );
}
