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
    apiToken?: string;
    /** When `apiToken` stops being accepted, in unix ms. The api mints short
     *  tokens and the session outlives them, so this is what says when to ask
     *  for another. */
    apiTokenExp?: number;
    username?: string;
  }
}
