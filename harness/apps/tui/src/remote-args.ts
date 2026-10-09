/** kl-tui's transport: `--ssh <ssh argv…>` (ssh is prefixed) or `--pipe <command…>` (run as given —
 * kl-connect's `bench-proxy --tui`, wss to the bench daemon). `null` = usage error. Split from
 * remote.tsx, which runs on import. */
export function pipeArgv(argv: string[]): string[] | null {
  for (const [flag, prefix] of [["--ssh", ["ssh"]], ["--pipe", []]] as const) {
    const i = argv.indexOf(flag);
    if (i >= 0) return i === argv.length - 1 ? null : [...prefix, ...argv.slice(i + 1)];
  }
  return null;
}
