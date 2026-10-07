import { useEffect, useState } from "react";

const FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/** Braille dots spinner (replaces ink-spinner). */
export function Spinner({ fg }: { fg?: string }) {
  const [i, setI] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setI((n) => (n + 1) % FRAMES.length), 80);
    return () => clearInterval(id);
  }, []);
  return <span fg={fg}>{FRAMES[i]}</span>;
}
