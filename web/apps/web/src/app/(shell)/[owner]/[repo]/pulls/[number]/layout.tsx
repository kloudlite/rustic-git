import { BackLink } from "@/components/repo/back-link";
import { PullHeader } from "@/components/repo/pull-page";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { pullData } from "./pull-data";

/** The header lives here rather than in each of the three tabs so it is not
 *  re-mounted on every switch — that is what made the underline blink instead of
 *  slide. `guardRepo` and `pullData` are both `cache()`d, so the pages below still
 *  call `pullData` for their bodies and share this one read. */
export default async function Layout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { pull, counts, diff } = await pullData(token, owner, repo, Number(number));

  return (
    <section className="min-w-0">
      <BackLink href={`/${owner}/${repo}/pulls`}>Pull requests</BackLink>
      <div className="mt-3">
        <PullHeader owner={owner} repo={repo} pull={pull} counts={counts} stat={diff} />
      </div>
      {children}
    </section>
  );
}
