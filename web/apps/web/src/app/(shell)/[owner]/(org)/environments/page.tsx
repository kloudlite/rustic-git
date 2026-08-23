import { requireSession } from "@/lib/session";
import { NotYet } from "@/components/app/not-yet";

export default async function Page() {
  await requireSession();
  return <NotYet title="Environments">Environments are not available yet.</NotYet>;
}
