import { redirect } from "next/navigation";

/** CI is managed at the team level and declared in each repo's `.actions/`; the
 *  runs for one repo are the team's CI Triggers page filtered to it. */
export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  redirect(`/${owner}/ci?repo=${repo}`);
}
