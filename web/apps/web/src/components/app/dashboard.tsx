import { RepoList } from "@/components/app/repo-list";
import { ActivityFeed } from "@/components/app/activity-feed";
import type { ApiEvent, ApiRepo } from "@/lib/api";

/** Home for a signed-in user is the Code Repos list. The section tab already names
 *  the page, so there is no title to repeat: one toolbar row — filter, scope, count,
 *  primary action — then the list. Every list page in the product shares this shape. */
export function Dashboard({
  owner,
  repos,
  events,
}: {
  owner: string;
  repos: ApiRepo[];
  events: ApiEvent[];
}) {
  return (
    <>
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
