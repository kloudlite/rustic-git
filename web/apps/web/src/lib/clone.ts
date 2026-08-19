/**
 * Where to clone from.
 *
 * Read on the SERVER and passed down as props, never through `NEXT_PUBLIC_`:
 * those are inlined at build time, so a value set in the deployment would never
 * reach the browser and the menu would keep printing whatever the image was built
 * with. The addresses a person copies must be the ones this deployment actually
 * answers on — a stale host is how a Clone menu ends up handing out URLs that
 * resolve to somebody else's site.
 *
 * The ssh form matters as much as the host. `git@host:owner/repo.git` is scp
 * syntax and cannot carry a port, so a deployment whose ssh listener is not on 22
 * must print `ssh://git@host:port/owner/repo.git` instead — the short form would
 * silently try port 22 and hang.
 */
export type CloneUrls = { https: string; ssh: string; cli: string };

export function cloneUrls(owner: string, repo: string): CloneUrls {
  const httpHost = process.env.RUSTIC_GIT_CLONE_HOST ?? "dev.kloudlite.io";
  const sshHost = process.env.RUSTIC_GIT_SSH_HOST ?? "git.khost.dev";
  const sshPort = Number(process.env.RUSTIC_GIT_SSH_PORT ?? 22);
  const path = `${owner}/${repo}.git`;
  return {
    https: `https://${httpHost}/${path}`,
    ssh: sshPort === 22 ? `git@${sshHost}:${path}` : `ssh://git@${sshHost}:${sshPort}/${path}`,
    cli: `kl clone ${owner}/${repo}`,
  };
}
