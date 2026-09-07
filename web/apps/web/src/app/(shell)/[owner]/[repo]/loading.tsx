import { Bone, Skeleton } from "@/components/app/skeleton";

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
          <Bone className="mt-5 h-5 w-48" />
          {/* The listing card: a 45px latest-commit header, then 37px `py-2` file rows. */}
          <div className="mt-3 border border-border bg-card">
            <div className="flex items-center gap-3 border-b border-border px-4 py-3">
              <Bone className="size-5 shrink-0" />
              <Bone className="h-5 w-64" />
              <Bone className="ml-auto h-4 w-24" />
            </div>
            {Array.from({ length: 6 }, (_, i) => (
              <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-2 last:border-b-0">
                <Bone className="size-4 shrink-0" />
                <Bone className="h-5 w-40" />
                <Bone className="ml-auto h-4 w-16" />
              </div>
            ))}
          </div>
        </section>
        <aside className="hidden xl:block">
          <Bone className="h-[18px] w-16" />
          <Bone className="mt-3 h-3 w-full" />
          <Bone className="mt-2 h-3 w-5/6" />
          <Bone className="mt-2 h-3 w-4/6" />
        </aside>
      </div>
    </Skeleton>
  );
}
