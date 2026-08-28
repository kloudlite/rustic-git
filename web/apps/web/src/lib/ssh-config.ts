/** The two things the workspace row hands a person to paste.
 *
 *  The block mirrors `bins/kl/src/sshconfig.rs::render` line for line: the CLI writes the
 *  same stanza into `~/.ssh/kloudlite_config`, and a copy that differs from it would leave
 *  the same host configured two ways depending on which one the person used. Kept as a pure
 *  function so the snippet and the copy button cannot drift apart.
 *
 *  `HostName` is the id, not the name: names are unique per person, ids are what the gateway
 *  routes on and what the host key is pinned under. */
/** The same rule the api enforces on create (`model::valid_ws_name`) and the CLI's renderer
 *  applies (`bins/kl/src/sshconfig.rs::safe_name`). A newline in a name appends arbitrary
 *  keywords to whatever this block is pasted into — a `ProxyCommand` under `Host *` runs on the
 *  reader's machine for every ssh they make — so a name that fails it gets no block at all. */
function safeName(s: string): boolean {
  return /^[A-Za-z0-9._-]{1,63}$/.test(s);
}

export function sshConfigBlock(name: string, id: string): string | null {
  // The id too: it is written into HostName and HostKeyAlias, and nothing here should trust
  // the shape of either string.
  if (!safeName(name) || !safeName(id)) return null;
  return (
    `Host ${name}\n` +
    `  HostName ${id}\n` +
    "  User kl\n" +
    `  ProxyCommand kl ws proxy ${id}\n` +
    "  UserKnownHostsFile ~/.config/kl/known_hosts\n" +
    `  HostKeyAlias ${id}\n`
  );
}

export function sshOneLiner(name: string): string {
  return `kl ws ssh ${name}`;
}
