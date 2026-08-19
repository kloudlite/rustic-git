import "server-only";
import { cookies } from "next/headers";
import { auth } from "@/auth";
import { DEV_BYPASS, DEV_SIGNED_OUT_COOKIE, devUser } from "@/lib/dev-auth";

export type Session = { user: { name: string; email: string; owner: string } } | null;

/** The single place pages ask who is signed in. */
export async function getSession(): Promise<Session> {
  const session = await auth();
  const user = session?.user;

  if (user?.email) {
    return {
      user: {
        name: user.name ?? user.email,
        email: user.email,
        owner: user.owner || user.email.split("@")[0],
      },
    };
  }

  /* Only reached with no real session, so signing in for real always wins over
     the bypass rather than being shadowed by it. */
  if (DEV_BYPASS) {
    const jar = await cookies();
    if (jar.get(DEV_SIGNED_OUT_COOKIE)?.value !== "1") return { user: devUser() };
  }

  return null;
}
