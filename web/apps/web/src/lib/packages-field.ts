/** The restore form's `packages` field, as the api wants it.
 *
 *  Presence is the whole rule, and it is decided HERE, once: an absent field means "use the
 *  definition the snapshot froze", and a present-but-blank field means "the snapshot had none,
 *  and that is what I accepted". Those two were being decided twice — by `has()` in the action
 *  and by truthiness in `restoreWorkspace` — and a tidy-up of either would have made a
 *  package-less restore silently inherit a list the person never saw. */
export function packagesField(fd: FormData): string[] | undefined {
  if (!fd.has("packages")) return undefined;
  return String(fd.get("packages")).split(",").map((p) => p.trim()).filter(Boolean);
}

export const MAX_PACKAGES = 100;
const MAX_ATTR_LEN = 64;
const ATTR = /^[A-Za-z0-9_][A-Za-z0-9_.+-]*$/;
/** devbox's grammar: `latest`, or one to three dotted numbers, digits only. */
const VERSION = /^(latest|[0-9]+(\.[0-9]+){0,2})$/;

/** The same rules as `crates/workspaces/src/packages.rs::validate_list`, checked here so a typo
 *  is answered by the field rather than by a round trip. NOT a substitute for the api's own
 *  check — the CR is not a trust boundary this form controls — just the fast half of it.
 *  Returns the sentence to show, or `null` when the list is fine. */
export function packageListError(list: string[]): string | null {
  if (list.length > MAX_PACKAGES) return `${list.length} packages; the limit is ${MAX_PACKAGES}`;
  const seen = new Set<string>();
  for (const p of list) {
    const at = p.indexOf("@");
    const attr = at === -1 ? p : p.slice(0, at);
    if (p.length > MAX_ATTR_LEN || !ATTR.test(attr)) return `"${p}" is not a package attribute name`;
    if (at !== -1 && !VERSION.test(p.slice(at + 1))) return `"${p}" is not a version: use latest, N, N.N or N.N.N`;
    // Keyed on the ATTR: `nodejs` and `nodejs@20` are two asks for one profile entry.
    if (seen.has(attr)) return `"${attr}" is listed twice`;
    seen.add(attr);
  }
  return null;
}
