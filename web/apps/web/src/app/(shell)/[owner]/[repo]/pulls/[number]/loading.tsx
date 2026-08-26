import { Bone, Skeleton } from "@/components/app/skeleton";

/** A pull: back link, the header (title, state, branches), tabs, then the conversation beside
 *  its sidebar on `grid-cols-overview`. Files and commits beneath share the header. */
export default function Loading() {
  return (
    <Skeleton>
      <Bone className="h-4 w-24" />
      <Bone className="mt-3 h-7 w-96 max-w-full" />
      <div className="mt-2 flex items-center gap-2">
        <Bone className="h-5 w-16" />
        <Bone className="h-4 w-64 max-w-full" />
      </div>
      <div className="mt-5 flex gap-6 border-b border-border pb-2">
        <Bone className="h-4 w-24" />
        <Bone className="h-4 w-16" />
        <Bone className="h-4 w-12" />
      </div>
      <div className="mt-6 grid gap-10 lg:grid-cols-overview">
        <section className="grid min-w-0 gap-8">
          {[0, 1].map((i) => (
            <div key={i} className="border border-border bg-card">
              <div className="flex items-center gap-3 border-b border-border px-4 py-3">
                <Bone className="size-6" />
                <Bone className="h-3 w-40" />
              </div>
              <div className="px-4 py-4">
                <Bone className="h-3 w-full" />
                <Bone className="mt-2 h-3 w-5/6" />
              </div>
            </div>
          ))}
        </section>
        <aside>
          <Bone className="h-3 w-20" />
          <Bone className="mt-3 h-9 w-full" />
          <Bone className="mt-6 h-3 w-24" />
          <Bone className="mt-3 h-4 w-32" />
        </aside>
      </div>
    </Skeleton>
  );
}
