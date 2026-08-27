import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { Landing } from "@/components/marketing/landing";

/** One route, two audiences. A signed-out visitor is being introduced to the
 *  product; a signed-in one belongs in a namespace — home is `/{owner}` now, and
 *  which owner that is depends on who is reading, so `/` sends them to their own. */
export default async function RootPage() {
  const session = await getSession();
  if (!session) return <Landing />;
  /* The handle is the URL, so it has to exist first. */
  if (!session.user.username) redirect("/welcome");
  redirect(`/${session.user.owner}`);
}
