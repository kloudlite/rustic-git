/** The names `/{owner}/…` has already spent on pages of the namespace.
 *
 *  Repo creation refuses these (see `RESERVED_REPO_NAMES` in the server), which
 *  is what makes `/{owner}/{x}` unambiguous: `x` names a section or it names a
 *  repo, and the chrome can tell which from the URL alone. Keep the two lists in
 *  step — a name here that the server allows would shadow a real repo.
 */
export const RESERVED = [
  "activity",
  "repos",
  "settings",
  "registries",
  "workspaces",
  "environments",
  "snapshots",
  // No page any more, but the server still refuses it as a repo name, and the two
  // lists must stay in step — dropping it here would let `/{owner}/ci` mean a repo
  // the server will never create.
  "ci",
] as const;
