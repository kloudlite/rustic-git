/** An email is how the directory names a person; a page is not the place to print
 *  one in full. Shared so the feed and the pull views name people identically. */
export function displayName(actor: string) {
  return actor.includes("@") ? actor.split("@")[0] : actor;
}
