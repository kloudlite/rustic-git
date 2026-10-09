/** kl-tui's transport: `--pipe <command…>`, run as given (kl-connect's `bench-proxy --tui`, wss to
 * the bench daemon). `null` = usage error. Split from remote.tsx, which runs on import. */
export function pipeArgv(argv: string[]): string[] | null {
  const i = argv.indexOf("--pipe");
  return i < 0 || i === argv.length - 1 ? null : argv.slice(i + 1);
}
