import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { loadEnvPage } from "@/lib/env-page";
import { EnvSettings } from "@/components/app/env-settings";

export const metadata: Metadata = { title: "Environment settings" };

export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  const token = await apiToken();
  if (!token) redirect("/login");

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  return <EnvSettings owner={owner} id={id} name={page.name} archived={!page.env} />;
}
