import { getSession } from "@/lib/session";
import { Landing } from "@/components/marketing/landing";
import { Dashboard } from "@/components/app/dashboard";

/** One route, two audiences. A signed-out visitor is being introduced to the product;
 *  a signed-in one wants to know what changed since they last looked. */
export default async function HomePage() {
  const session = await getSession();
  return session ? <Dashboard session={session} /> : <Landing />;
}
