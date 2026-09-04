import { notFound, redirect } from "next/navigation";
import { images } from "@/lib/browse";
import { ImageList } from "@/components/app/image-list";
import { requireToken } from "@/lib/session";
import { registryHost } from "@/lib/clone";

/** The tab the "Container Images" nav entry has always pointed at. An image
 *  appears here by being pushed — there is no create button — so this page's
 *  job on an empty team is to hand over the three lines that make one. */
export default async function RegistriesPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const { token } = await requireToken(`/${owner}/registries`);

  const list = await images(token, owner);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  const host = await registryHost();

  // Full page width, like every other list in the namespace — the section tab
  // already names the page, so there is no title to repeat.
  return (
    <section>
      <ImageList owner={owner} host={host} images={list.value} />
    </section>
  );
}
