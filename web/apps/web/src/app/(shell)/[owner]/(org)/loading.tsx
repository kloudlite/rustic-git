import { Bone, FeedBones, LineBones, Skeleton } from "@/components/app/skeleton";

/** `/{owner}`: home.tsx — the title row, then two stacked sections (workspaces,
 *  activity) beside the rail (environments, repos) on `grid-cols-overview`. Each section
 *  is a 12px `text-caption` heading (`h-3`) over `mt-3` rows of `px-4 py-3`, which is
 *  what `LineBones` draws.
 *
 *  This file serves ONLY the group's index page in practice: every child route under
 *  `(org)` carries its own loading.tsx, because Next picks the nearest one and this
 *  would otherwise paint the home page's shape over a list page. */
export default function Loading() {
  return (
    <Skeleton>
      <div className="flex items-start justify-between gap-4">
        <div>
          {/* Title (`text-title`) over the `mt-1` subtitle line. */}
          <Bone className="h-7 w-48" />
          <Bone className="mt-2 h-4 w-72" />
        </div>
        {/* The "View as" switch, `h-8` like the page. Teams only, but it is the wider
            case and a rail that is one control short moves less than one too many. */}
        <Bone className="h-8 w-36" />
      </div>
      <div className="mt-8 grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <Bone className="h-3 w-32" />
          <LineBones rows={3} className="mt-3" />
          <div className="mt-8">
            <Bone className="h-3 w-28" />
            {/* The feed's day heading, then its rows: `ActivityFeed` opens on `mt-4`. */}
            <Bone className="mt-3 h-4 w-16" />
            <FeedBones rows={6} className="mt-4" />
          </div>
        </section>
        <aside className="grid content-start gap-8">
          {/* Environments, then the repos rail: the team's environments are a rail
              block, not a second list under the workspaces. */}
          <div>
            <Bone className="h-3 w-28" />
            <LineBones rows={3} className="mt-3" />
          </div>
          <div>
            <Bone className="h-3 w-16" />
            <LineBones rows={5} className="mt-3" />
          </div>
        </aside>
      </div>
    </Skeleton>
  );
}
