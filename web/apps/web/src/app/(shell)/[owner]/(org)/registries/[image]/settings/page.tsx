import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { imageTags } from "@/lib/browse";
import { ImageSettings } from "@/components/registry/image-settings";

export const metadata: Metadata = { title: "Image settings" };

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; image: string }>;
}) {
  const { owner, image } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const tags = await imageTags(token, owner, image);
  if (!tags.ok) {
    if (tags.kind === "unauthorized") redirect("/login?from=expired");
    if (tags.kind === "notFound") notFound();
    throw new Error(tags.message);
  }

  return <ImageSettings owner={owner} image={image} tags={tags.value} />;
}
