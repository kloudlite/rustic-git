import { Boxes, Laptop } from "lucide-react";
import { RepoList } from "@/components/app/repo-list";
import { ActivityFeed } from "@/components/app/activity-feed";
import { ViewAs } from "@/components/app/view-as";
import type { ApiEnvironment, ApiEvent, ApiRepo, ApiWorkspace } from "@/lib/api";

/** Home for a signed-in member: a switch back to the public view (teams only — your own
 *  handle has no public half), a strip of what is running right now, then the Code Repos
 *  list beside the activity rail. The section tab already names the page, so there is no
 *  title to repeat, and the list keeps the same toolbar row every list page here has. */
export function TeamOverview({
  owner,
  repos,
  events,
  workspaces,
  environments,
  canSwitch,
}: {
  owner: string;
  repos: ApiRepo[];
  events: ApiEvent[];
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
  canSwitch: boolean;
}) {
  // "ready" is a workspace's running state; environments say so outright.
  const running = [
    ...workspaces.filter((w) => w.state === "ready").map((w) => ({ id: `ws-${w.id}`, name: w.name, icon: Laptop })),
    ...environments.filter((e) => e.state === "running").map((e) => ({ id: `env-${e.id}`, name: e.name, icon: Boxes })),
  ];

  return (
    <>
      {canSwitch && (
        <div className="mb-4 flex h-8 items-center justify-end">
          <ViewAs slug={owner} view="member" />
        </div>
      )}

      {/* Nothing running is the normal state, not an error, and the repo list is what
          the page is for — so the strip is absent rather than empty. */}
      {running.length > 0 && (
        <div className="mb-6 flex flex-wrap gap-3">
          {running.map(({ id, name, icon: Icon }) => (
            <div key={id} className="flex items-center gap-2 border border-border bg-card px-4 py-3">
              <Icon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
              <span className="truncate text-sm2">{name}</span>
            </div>
          ))}
        </div>
      )}

      <div className="grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <RepoList owner={owner} repos={repos} />
        </section>

        <aside>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
            Activity
          </h2>
          <ActivityFeed events={events} more={`/${owner}/activity`} />
        </aside>
      </div>
    </>
  );
}
