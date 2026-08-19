import { AppShell } from "@/components/app/app-shell";
import { REPO } from "@/lib/mock-repo";
import type { Session } from "@/lib/session";

/** Every repo page: the shell in repo context, with the repo's tabs. */
export function RepoPage({
  session,
  active,
  children,
}: {
  session: NonNullable<Session>;
  active: "Code" | "Issues" | "Pull requests" | "Actions" | "Settings";
  children: React.ReactNode;
}) {
  return (
    <AppShell
      session={session}
      active={active}
      context={{ kind: "repo", name: REPO.name, visibility: REPO.visibility }}
    >
      <main className="mx-auto max-w-page px-6 pt-6 pb-16">{children}</main>
    </AppShell>
  );
}
