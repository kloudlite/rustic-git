import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { page } from "@/lib/docs";
import { DocsPage } from "@/components/docs/docs-page";

type Params = { params: Promise<{ slug: string[] }> };

export async function generateMetadata({ params }: Params): Promise<Metadata> {
  const { slug } = await params;
  const p = await page(slug);
  return p ? { title: p.title, description: p.description } : {};
}

export default async function DocsSlugPage({ params }: Params) {
  const { slug } = await params;
  if (!(await page(slug))) notFound();
  return <DocsPage slug={slug} />;
}
