import type { Metadata } from "next";
import { requireSession } from "@/lib/session";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { getBenchSession } from "./actions";

export const metadata: Metadata = { title: "Bench" };

export default async function BenchPage() {
  await requireSession("/bench");
  const r = await getBenchSession();

  if ("error" in r) {
    return (
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <p className="text-muted-foreground">{r.error}</p>
      </main>
    );
  }

  if ("state" in r) {
    return (
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <p className="text-muted-foreground">Starting your bench ({r.state})…</p>
        <AutoRefresh intervalMs={2_000} />
      </main>
    );
  }

  return <iframe src={r.termUrl} title="Bench terminal" className="h-full w-full border-0" />;
}
