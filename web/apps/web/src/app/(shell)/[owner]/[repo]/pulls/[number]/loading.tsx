import { Bone, Skeleton } from "@/components/app/skeleton";

/** The conversation BODY only: the back link and the header live in the segment's
 *  layout, which sits above this boundary and is already painted when it shows.
 *  The body is the conversation beside its sidebar on `grid-cols-overview`. */
export default function Loading() {
  return (
    <Skeleton>
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
