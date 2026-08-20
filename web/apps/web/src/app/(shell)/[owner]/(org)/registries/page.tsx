import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { images } from "@/lib/browse";
import { ImageList } from "@/components/app/image-list";

/** The tab the "Container Images" nav entry has always pointed at. An image
 *  appears here by being pushed — there is no create button — so this page's
 *  job on an empty team is to hand over the three lines that make one. */
export default async function RegistriesPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const list = await images(token, owner);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  const host = (process.env.RUSTIC_GIT_CLONE_HOST ?? "cr.khost.dev").replace(/\/$/, "");

  return (
    <section className="mx-auto max-w-2xl">
      <h1 className="text-title font-semibold tracking-title">Container Images</h1>
      <p className="mt-2 text-sm2 text-muted-foreground">
        Images {owner} has pushed to this registry.
      </p>
      <ImageList owner={owner} host={host} images={list.value} />
    </section>
  );
}
