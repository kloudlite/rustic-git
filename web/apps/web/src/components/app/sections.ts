import { Container, House, Layers, Settings, SquareCode, SquareTerminal, Zap, type LucideIcon } from "lucide-react";

/** One entry in the tab row. `exact` is what stops Home — whose href is a prefix of
 *  every other section — from staying active on all of them. */
export type Section = { href: string; label: string; icon: LucideIcon; exact?: boolean };

/** Home — the namespace itself — then the five parts of the product in the order work moves through
 *  them: the code, the place it is worked on, the place it runs, what builds it,
 *  and what that build produced. Section routes hang off the owner, so this is a
 *  function of it. */
export function sections(owner: string): Section[] {
  return [
    // Home is the namespace itself, so it matches that URL exactly — without `exact`
    // it is a prefix of every other section and would never stop being the active tab.
    { href: `/${owner}`, label: "Home", icon: House, exact: true },
    { href: `/${owner}/repos`, label: "Code Repos", icon: SquareCode },
    { href: `/${owner}/workspaces`, label: "Workspaces", icon: SquareTerminal },
    // No Snapshots tab: an environment's snapshots live on its own row (they are what that
    // environment is), and a workspace's live on its row and nowhere else — they are that one
    // person's undo history, not a shared listing. `snapshots` stays a RESERVED repo name.
    { href: `/${owner}/environments`, label: "Environments", icon: Layers },
    { href: `/${owner}/ci`, label: "CI Triggers", icon: Zap },
    // The URL is still `registries`: renaming it would have to reserve `images`
    // as a repo name, and that is a name someone will want for a repo.
    { href: `/${owner}/registries`, label: "Container Images", icon: Container },
  ];
}

/** Team settings sit apart from the product sections — at the far end of the row. */
export function settingsSection(owner: string): Section {
  return { href: `/${owner}/settings`, label: "Team settings", icon: Settings };
}
