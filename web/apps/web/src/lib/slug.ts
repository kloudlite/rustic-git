/** An owner or repo segment safe to build a `revalidatePath()` from.
 *
 *  `revalidatePath` takes a PATTERN, so a segment carrying `/`, `[`, `]` or `..` does not just
 *  fail — it silently revalidates something else, or nothing. Server actions read these from
 *  FormData, which is client-controlled, so every one is checked here before it becomes a path.
 *  The rule matches the server's own `valid_segment` (`crates/storage/src/store.rs`): ASCII
 *  letters, digits, `-`, `_`, `.`, 1–100 chars, never `.` or `..` alone. */
export function safeSegment(s: string): string | null {
  if (s.length === 0 || s.length > 100 || s === "." || s === "..") return null;
  return /^[A-Za-z0-9._-]+$/.test(s) ? s : null;
}

/** Both segments, or null if either fails. The shape every repo-scoped action needs. */
export function safeRepoPath(owner: string, repo: string): { owner: string; repo: string } | null {
  const o = safeSegment(owner);
  const r = safeSegment(repo);
  return o && r ? { owner: o, repo: r } : null;
}
