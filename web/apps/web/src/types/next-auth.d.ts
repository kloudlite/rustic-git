import type { DefaultSession } from "next-auth";

declare module "next-auth" {
  interface Session {
    user: { owner: string } & DefaultSession["user"];
  }
}
