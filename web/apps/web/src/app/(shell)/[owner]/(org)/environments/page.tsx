import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { listEnvironments } from "@/lib/api";
import { EnvironmentList } from "@/components/app/environment-list";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  // On the caller's OWN page, no owner filter: the api then aggregates personal envs plus
  // every team the caller belongs to — environments are a team-wide view. A team's page
  // keeps the filter so it shows exactly that team's.
  const mine = owner === session.user.username;
  const list = await listEnvironments(token, mine ? undefined : owner);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  return (
    <section>
      <EnvironmentList owner={owner} environments={list.value} />
    </section>
  );
}
