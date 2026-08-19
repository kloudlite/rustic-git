import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { Landing } from "@/components/marketing/landing";
import { Home } from "@/components/app/home";

/** One route, two audiences. A signed-out visitor is being introduced to the
 *  product; a signed-in one gets the team feed — what changed since they last looked. */
export default async function HomePage() {
  const session = await getSession();
  if (!session) return <Landing />;
  /* Everything past here builds URLs from the handle, so it has to exist first. */
  if (!session.user.username) redirect("/welcome");
  return <Home session={session} />;
}
