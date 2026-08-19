import { clsx, type ClassValue } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

/** tailwind-merge only knows Tailwind's stock utilities. Our type scale adds
 *  `text-sm2`, `text-title` and friends; without being told they are font sizes,
 *  the merger files them as text *colours* and drops one whenever it meets
 *  `text-muted-foreground` in the same class list. The size silently vanished and
 *  the element rendered at 16px — every "font is too big" report traced here.
 *
 *  Keep this list in step with the --text-* tokens in globals.css. */
const twMerge = extendTailwindMerge({
  extend: {
    classGroups: {
      "font-size": [
        "text-micro",
        "text-caption",
        "text-sm2",
        "text-body",
        "text-lead",
        "text-title",
        "text-display",
      ],
      tracking: ["tracking-eyebrow", "tracking-label", "tracking-title", "tracking-display"],
      leading: ["leading-title", "leading-display"],
    },
  },
});

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
