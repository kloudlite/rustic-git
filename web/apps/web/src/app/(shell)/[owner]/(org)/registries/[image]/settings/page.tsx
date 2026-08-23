import type { Metadata } from "next";
import { guardImage } from "../guard";
import { ImageSettings } from "@/components/registry/image-settings";

export const metadata: Metadata = { title: "Image settings" };

export default async function Page({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags } = await guardImage(owner, image);
  return <ImageSettings owner={owner} image={image} tags={tags} />;
}
