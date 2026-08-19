import { House, Layers, Package, SquareCode, SquareTerminal, Zap, type LucideIcon } from "lucide-react";

/** Home, then the five parts of the product in the order the landing page names
 *  them. Section routes hang off the owner, so this is a function of it. */
export function sections(owner: string): { href: string; label: string; icon: LucideIcon }[] {
  return [
    { href: "/", label: "Home", icon: House },
    { href: `/${owner}`, label: "Code Repos", icon: SquareCode },
    { href: `/${owner}/registries`, label: "Package Registries", icon: Package },
    { href: `/${owner}/workspaces`, label: "Workspaces", icon: SquareTerminal },
    { href: `/${owner}/environments`, label: "Environments", icon: Layers },
    { href: `/${owner}/ci`, label: "CI Triggers", icon: Zap },
  ];
}
