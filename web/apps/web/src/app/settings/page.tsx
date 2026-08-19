import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { UserSettings } from "@/components/app/user-settings";

export const metadata: Metadata = { title: "Settings" };

export default async function Page() {
  const session = await getSession();
  if (!session) redirect("/login");
  return <UserSettings session={session} />;
}
