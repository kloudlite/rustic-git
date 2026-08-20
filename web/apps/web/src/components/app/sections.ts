import { Container, House, Layers, Settings, SquareCode, SquareTerminal, Zap, type LucideIcon } from "lucide-react";

/** Home, then the five parts of the product in the order work moves through
 *  them: the code, the place it is worked on, the place it runs, what builds it,
 *  and what that build produced. Section routes hang off the owner, so this is a
 *  function of it. */
export function sections(owner: string): { href: string; label: string; icon: LucideIcon }[] {
  return [
    { href: "/", label: "Home", icon: House },
    { href: `/${owner}`, label: "Code Repos", icon: SquareCode },
    { href: `/${owner}/workspaces`, label: "Workspaces", icon: SquareTerminal },
    { href: `/${owner}/environments`, label: "Environments", icon: Layers },
    { href: `/${owner}/ci`, label: "CI Triggers", icon: Zap },
    // The URL is still `registries`: renaming it would have to reserve `images`
    // as a repo name, and that is a name someone will want for a repo.
    { href: `/${owner}/registries`, label: "Container Images", icon: Container },
  ];
}

/** Team settings sit apart from the product sections — at the far end of the row. */
export function settingsSection(owner: string): { href: string; label: string; icon: LucideIcon } {
  return { href: `/${owner}/settings`, label: "Settings", icon: Settings };
}
