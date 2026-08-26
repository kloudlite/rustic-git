import { Bone, LineBones, Skeleton } from "@/components/app/skeleton";

/** `/`: home.tsx renders its own page container, so this does as well — a skeleton without it
 *  sat flush-left and full-width, then jumped into the column when the page landed. Title,
 *  the tab row, then feed beside the repo rail on the same `grid-cols-overview` token. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton>
        <div className="grid gap-10 xl:grid-cols-overview">
          <section className="min-w-0">
            <Bone className="h-7 w-48" />
            <div className="mt-4 flex gap-6 border-b border-border pb-2">
              <Bone className="h-4 w-16" />
              <Bone className="h-4 w-20" />
              <Bone className="h-4 w-24" />
            </div>
            <LineBones rows={8} className="mt-5" />
          </section>
          <aside>
            <Bone className="h-3 w-24" />
            <LineBones rows={5} className="mt-4" />
          </aside>
        </div>
      </Skeleton>
    </main>
  );
}
