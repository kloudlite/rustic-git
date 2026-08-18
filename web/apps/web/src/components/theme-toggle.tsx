"use client";

import { useTheme } from "next-themes";
import { useEffect, useState } from "react";
import { Monitor, Moon, Sun } from "lucide-react";
import { cn } from "@/lib/utils";

const OPTIONS = [
  { value: "light", icon: Sun, label: "Light" },
  { value: "system", icon: Monitor, label: "System" },
  { value: "dark", icon: Moon, label: "Dark" },
] as const;

/** A segmented control rather than a toggle: "system" is a real third state, not the
 *  absence of a choice, and hiding it behind a cycle makes it undiscoverable. */
export function ThemeToggle({ className }: { className?: string }) {
  const { theme, setTheme } = useTheme();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);

  return (
    <div className={cn("inline-flex h-7 items-center border border-foreground/[0.08] p-0.5", className)} role="group" aria-label="Theme">
      {OPTIONS.map(({ value, icon: Icon, label }) => {
        const active = mounted && theme === value;
        return (
          <button
            key={value}
            type="button"
            onClick={() => setTheme(value)}
            aria-label={label}
            aria-pressed={active}
            className={cn(
              "flex h-6 w-7 items-center justify-center transition-colors",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-0",
              active
                ? "bg-foreground/[0.06] text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            <Icon className="size-3" />
          </button>
        );
      })}
    </div>
  );
}
