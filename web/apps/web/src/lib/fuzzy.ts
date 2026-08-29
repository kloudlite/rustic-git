/** Subsequence match with a score that prefers runs, word starts and short
 *  paths — enough to make "srau" land on src/auth.rs. Returns null on no match. */
export function fuzzy(query: string, path: string): { score: number; hits: number[] } | null {
  const q = query.toLowerCase();
  const p = path.toLowerCase();
  let qi = 0, score = 0, prev = -2;
  const hits: number[] = [];
  for (let i = 0; i < p.length && qi < q.length; i++) {
    if (p[i] !== q[qi]) continue;
    const boundary = i === 0 || "/._-".includes(p[i - 1]);
    score += 10 + (i === prev + 1 ? 15 : 0) + (boundary ? 20 : 0);
    hits.push(i); prev = i; qi++;
  }
  if (qi < q.length) return null;
  const last = p.lastIndexOf("/");
  const inName = hits.filter((h) => h > last).length;
  return { score: score + inName * 8 - p.length * 0.3, hits };
}
