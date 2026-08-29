import "server-only";
import { cache } from "react";
import { notFound, redirect } from "next/navigation";
import { imageTags, type ImageTag } from "@/lib/browse";
import { requireToken } from "@/lib/session";

export type ImageContext = { owner: string; image: string; token: string; tags: ImageTag[] };

/** Every image route: signed in, and this image exists in a namespace the caller
 *  may act in — the api answers 404 otherwise, so asking is the check. Wrapped in
 *  `cache` so the layout and the page beneath it resolve the image ONCE per
 *  request, the way `guardRepo` does for a repo. */
export const guardImage = cache(async function guardImage(owner: string, image: string): Promise<ImageContext> {
  const { token } = await requireToken(`/${owner}/registries/${image}`);

  const tags = await imageTags(token, owner, image);
  if (!tags.ok) {
    if (tags.kind === "unauthorized") redirect("/login?from=expired");
    if (tags.kind === "notFound") notFound();
    throw new Error(tags.message);
  }
  return { owner, image, token, tags: tags.value };
});
