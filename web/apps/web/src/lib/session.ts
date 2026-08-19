import "server-only";
import { auth } from "@/auth";

export type Session = { user: { name: string; email: string; owner: string } } | null;

/** The single place pages ask who is signed in. Shape is unchanged from the stub
 *  it replaced, so every caller kept working. */
export async function getSession(): Promise<Session> {
  const session = await auth();
  const user = session?.user;
  if (!user?.email) return null;
  return {
    user: {
      name: user.name ?? user.email,
      email: user.email,
      owner: user.owner || user.email.split("@")[0],
    },
  };
}
