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
  "ci",
] as const;
