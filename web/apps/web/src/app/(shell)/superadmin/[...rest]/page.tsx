import { notFound } from "next/navigation";

// `/superadmin/<anything else>` must be a 404 here, not a fall-through: without this catch-all,
// Next.js matches the org route `/[owner]/settings` with owner = "superadmin" and renders a team
// page under the superadmin path. The server also reserves the owner name, so no such team can
// ever exist — this is the belt to that brace.
export default function SuperadminCatchAll() {
  notFound();
}
