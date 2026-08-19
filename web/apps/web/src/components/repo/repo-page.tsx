import { AppShell } from "@/components/app/app-shell";
import type { Session } from "@/lib/session";

/** Every repo page: the shell in repo context, with the repo's tabs. */
export function RepoPage({
  session,
  repo,
  visibility,
  active,
  children,
}: {
  session: NonNullable<Session>;
  repo: string;
  visibility: "public" | "private";
  active: "Code" | "Issues" | "Pull requests" | "Actions" | "Settings";
  children: React.ReactNode;
}) {
  return (
    <AppShell session={session} active={active} context={{ kind: "repo", name: repo, visibility }}>
      <main className="mx-auto max-w-page px-6 pt-6 pb-16">{children}</main>
    </AppShell>
  );
}
