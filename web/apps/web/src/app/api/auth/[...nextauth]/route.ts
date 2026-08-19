import { handlers } from "@/auth";

/* The one route that must exist: OAuth providers redirect back to a URL, and a
   redirect cannot target a server action. No application data is served here. */
export const { GET, POST } = handlers;
