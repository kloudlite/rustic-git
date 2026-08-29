import type { DefaultSession } from "next-auth";

declare module "next-auth" {
  interface Session {
    user: {
      /** The handle they picked. Absent until they have. */
      username?: string;
    } & DefaultSession["user"];
  }
}

declare module "next-auth/jwt" {
  interface JWT {
    /** Minted at sign-in and never refreshed: the session's `maxAge` is the token's life. */
    apiToken?: string;
    username?: string;
  }
}
