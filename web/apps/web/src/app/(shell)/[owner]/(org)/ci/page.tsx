import { requireSession } from "@/lib/session";
import { NotYet } from "@/components/app/not-yet";

export default async function Page() {
  await requireSession();
  return <NotYet title="CI Triggers">CI triggers are not available yet.</NotYet>;
}
