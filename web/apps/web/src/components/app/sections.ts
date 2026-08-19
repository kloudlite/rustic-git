import { Layers, Package, SquareCode, SquareTerminal, Zap, type LucideIcon } from "lucide-react";

/** The five parts of the product, in the order the landing page names them.
 *  One list, shared by the sidebar and the mobile drawer. */
export const SECTIONS: { href: string; label: string; icon: LucideIcon }[] = [
  { href: "/", label: "Code Repos", icon: SquareCode },
  { href: "/kloudlite/registries", label: "Package Registries", icon: Package },
  { href: "/kloudlite/workspaces", label: "Workspaces", icon: SquareTerminal },
  { href: "/kloudlite/environments", label: "Environments", icon: Layers },
  { href: "/kloudlite/ci", label: "CI Triggers", icon: Zap },
];
