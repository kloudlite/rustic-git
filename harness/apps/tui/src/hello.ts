//! The backend the TUI was booted with, and what it said at hello. Set once by cli.tsx (local) or
//! remote.tsx (over ssh) BEFORE app.tsx is imported: models.ts and theme.ts read it at import.
import type { Backend, Hello } from "@kloudlite-tui/backend";

let b: Backend | undefined;
let h: Hello | undefined;

export function boot(backend: Backend, hi: Hello) {
  b = backend;
  h = hi;
}
export function hello(): Hello {
  if (!h) throw new Error("hello() before boot");
  return h;
}
export function backend(): Backend {
  if (!b) throw new Error("backend() before boot");
  return b;
}
