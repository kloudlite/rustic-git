"use client";

import { useEffect, useRef, useState } from "react";

/** Copy a value and say so for a moment. One place for the timer, so it is
 *  cleared on unmount — seven widgets each had their own, none of them cleared. */
export function useCopy(ms = 1600) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const copy = async (value: string) => {
    // A denied or absent clipboard is the browser's answer, not a crash: say
    // nothing rather than leave an unhandled rejection behind the click.
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      return;
    }
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), ms);
  };
  return { copied, copy };
}
