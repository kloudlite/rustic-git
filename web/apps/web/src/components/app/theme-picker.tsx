"use client";

import { useEffect, useState } from "react";
import { Monitor, Moon, Sun } from "lucide-react";
import { useTheme } from "next-themes";
import { cn } from "@/lib/utils";

const OPTIONS = [
  { value: "light", label: "Light", icon: Sun, hint: "Always light" },
  { value: "dark", label: "Dark", icon: Moon, hint: "Always dark" },
  { value: "system", label: "System", icon: Monitor, hint: "Follow the OS" },
] as const;

/** Theme as a setting: three options with a preview swatch, so the choice reads
 *  before it is made. Lives here rather than in the avatar menu — it is a
 *  preference, and preferences have a page. */
export function ThemePicker() {
  const { theme, setTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);

  return (
    <div role="radiogroup" aria-label="Theme" className="grid max-w-md grid-cols-3 gap-3">
      {OPTIONS.map(({ value, label, icon: Icon, hint }) => {
        const selected = mounted && theme === value;
        return (
          <button
            key={value}
            type="button"
            role="radio"
            aria-checked={selected}
            onClick={() => setTheme(value)}
            className={cn(
              "group grid gap-2 border p-2 text-left transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring",
              selected ? "border-primary" : "border-edge hover:border-edge-hover",
            )}
          >
            <span
              aria-hidden
              className={cn(
                "grid h-16 grid-rows-swatch gap-1 border p-1.5",
                value === "dark" ? "border-swatch-dark-edge bg-swatch-dark" : value === "light" ? "border-swatch-light-edge bg-swatch-light" : "border-swatch-light-edge bg-gradient-to-r from-swatch-light to-swatch-dark",
              )}
            >
              <span className={cn("h-full w-1/2", value === "dark" ? "bg-swatch-dark-ui" : "bg-swatch-light-ui")} />
              <span className={cn("h-full w-full", value === "dark" ? "bg-swatch-dark-ui/70" : "bg-swatch-light-ui/70")} />
            </span>
            <span className="flex items-center gap-1.5 px-0.5 text-sm2 font-medium">
              <Icon className="size-3.5 text-muted-foreground" /> {label}
            </span>
            <span className="px-0.5 text-caption text-muted-foreground">{hint}</span>
          </button>
        );
      })}
    </div>
  );
}
