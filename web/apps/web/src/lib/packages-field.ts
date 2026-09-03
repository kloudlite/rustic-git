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
