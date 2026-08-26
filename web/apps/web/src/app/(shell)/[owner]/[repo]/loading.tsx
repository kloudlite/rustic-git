import { Bone, LineBones, Skeleton } from "@/components/app/skeleton";

/** The code view — `/{owner}/{repo}`, `/tree/…`, `/blob/…`: ref picker and actions, the path
 *  crumb, then the listing beside the README rail on `grid-cols-code-rail`. The routes with a
 *  different shape (commits, pulls, a pull, a commit, settings) carry their own. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="grid gap-10 xl:grid-cols-code-rail">
        <section className="min-w-0">
          <div className="flex flex-wrap items-center gap-3">
            <Bone className="h-8 w-36" />
            <Bone className="h-8 w-24" />
            <Bone className="ml-auto h-8 w-24" />
          </div>
          <Bone className="mt-5 h-4 w-48" />
          <LineBones rows={8} className="mt-3" />
        </section>
        <aside className="hidden xl:block">
          <Bone className="h-4 w-24" />
          <Bone className="mt-3 h-3 w-full" />
          <Bone className="mt-2 h-3 w-5/6" />
          <Bone className="mt-2 h-3 w-4/6" />
        </aside>
      </div>
    </Skeleton>
  );
}
