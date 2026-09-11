import type { Metadata } from "next";
import { nav, searchIndex } from "@/lib/docs";
import { DocsHeader } from "@/components/docs/docs-header";
import { Sidebar } from "@/components/docs/sidebar";
import "./docs.css";

export const metadata: Metadata = {
  title: { default: "Kloudlite Docs", template: "%s · Kloudlite Docs" },
  description: "Workspaces, environments, snapshots and connections: how Kloudlite shortens the change → observe loop.",
};

/** The documentation site: its own chrome, no app shell. Three columns from 1280 px — the
 *  section tree, the page, the page's own outline — and the page alone below that, with the
 *  tree behind the header's menu. */
export default async function DocsLayout({ children }: { children: React.ReactNode }) {
  const [sections, index] = await Promise.all([nav(), searchIndex()]);
  return (
    <div className="docs min-h-screen bg-background text-foreground">
      <DocsHeader index={index} sections={sections} />
      <div className="docs-frame">
        <aside className="docs-side">
          <Sidebar sections={sections} />
        </aside>
        {children}
      </div>
    </div>
  );
}
