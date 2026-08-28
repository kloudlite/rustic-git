/** The two things the workspace row hands a person to paste.
 *
 *  The block mirrors `bins/kl/src/sshconfig.rs::render` line for line: the CLI writes the
 *  same stanza into `~/.ssh/kloudlite_config`, and a copy that differs from it would leave
 *  the same host configured two ways depending on which one the person used. Kept as a pure
 *  function so the snippet and the copy button cannot drift apart.
 *
 *  `HostName` is the id, not the name: names are unique per person, ids are what the gateway
 *  routes on and what the host key is pinned under. */
export function sshConfigBlock(name: string, id: string): string {
  return (
    `Host ${name}\n` +
    `  HostName ${id}\n` +
    "  User root\n" +
    `  ProxyCommand kl ws proxy ${id}\n` +
    "  UserKnownHostsFile ~/.config/kl/known_hosts\n" +
    `  HostKeyAlias ${id}\n`
  );
}

export function sshOneLiner(name: string): string {
  return `kl ws ssh ${name}`;
}
