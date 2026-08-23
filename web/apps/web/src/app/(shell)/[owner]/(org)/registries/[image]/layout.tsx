import { guardImage } from "./guard";

/** Refusing here means no page under it has to check; the page frame itself comes
 *  from `(org)/layout.tsx`, which already wraps this. */
export default async function ImageLayout({
  params,
  children,
}: {
  params: Promise<{ owner: string; image: string }>;
  children: React.ReactNode;
}) {
  const { owner, image } = await params;
  await guardImage(owner, image);
  return <>{children}</>;
}
