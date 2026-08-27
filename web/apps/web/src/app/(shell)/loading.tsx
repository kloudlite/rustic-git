import { Bone, FeedBones, LineBones, Skeleton, TitleBones } from "@/components/app/skeleton";

/** `/`: home.tsx renders its own page container, so this does as well — a skeleton without it
 *  sat flush-left and full-width, then jumped into the column when the page landed. Title, then
 *  the three stacked sections (workspaces, environments, activity) beside the teams rail on the
 *  same `grid-cols-overview` token. Each section is a 12px `text-caption` heading (`h-3`) over
 *  `mt-3` rows of `px-4 py-3`, which is what `LineBones` draws. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton>
        <div className="grid gap-10 xl:grid-cols-overview">
          <section className="min-w-0">
            <TitleBones width="w-32" subtitle={false} />
            <div className="mt-8">
              <Bone className="h-3 w-32" />
              <LineBones rows={3} className="mt-3" />
            </div>
            <div className="mt-8">
              <Bone className="h-3 w-36" />
              <LineBones rows={3} className="mt-3" />
            </div>
            <div className="mt-8">
              <Bone className="h-3 w-28" />
              {/* The feed's day heading, then its rows: `ActivityFeed` opens on `mt-4`. */}
              <Bone className="mt-3 h-3 w-16" />
              <FeedBones rows={6} className="mt-4" />
            </div>
          </section>
          <aside>
            <Bone className="h-3 w-16" />
            {/* The rail's teams plus the "New team" row that always follows them. */}
            <LineBones rows={4} className="mt-3" />
          </aside>
        </div>
      </Skeleton>
    </main>
  );
}
