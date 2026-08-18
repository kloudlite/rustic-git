import "server-only";
import { cookies } from "next/headers";

export type Session = { user: { name: string; email: string; owner: string } } | null;

/**
 * Stubbed until Auth.js lands. Reading a cookie keeps the call shape identical to what
 * `auth()` will return, so every page that uses it stays unchanged when it is swapped.
 * Set `kl_demo=1` in the browser to see the signed-in view.
 */
export async function getSession(): Promise<Session> {
  const jar = await cookies();
  if (jar.get("kl_demo")?.value !== "1") return null;
  return { user: { name: "Alice Chen", email: "alice@kloudlite.io", owner: "kloudlite" } };
}
