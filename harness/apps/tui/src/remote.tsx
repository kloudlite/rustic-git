#!/usr/bin/env bun
// kl-tui: the TUI on the laptop, the agent on the bench. `kl-tui --ssh <ssh argv...>`; kl-connect
// builds the argv and treats exit 3 (old bench, protocol mismatch) as "run the remote TUI".
import { writeSync } from "node:fs";
import { connect } from "@kloudlite-tui/backend";
import { boot } from "./hello.ts";
import { start } from "./start.tsx";

const i = process.argv.indexOf("--ssh");
if (i < 0 || i === process.argv.length - 1) {
  process.stderr.write("usage: kl-tui --ssh <ssh arguments...>\n");
  process.exit(2);
}
let c: Awaited<ReturnType<typeof connect>>;
try {
  c = await connect(["ssh", ...process.argv.slice(i + 1)]);
} catch (e: any) {
  process.stderr.write(`kl-tui: ${e.message}\n`);
  process.exit(e.code ?? 1);
}
boot(c.backend, c.hello);
let lost = false;
const done = () => {
  // Leave the alternate screen first or the reason goes with it; kl-connect then skips its own
  // `?1049l`, which would put the cursor back over this line. writeSync because an async write in
  // an exit handler can be dropped.
  if (process.stderr.isTTY) writeSync(2, "\x1b[?1049l");
  const tail = c.stderr();
  if (tail) writeSync(2, tail);
  // ssh writes nothing when its proxy dies, so a bare exit 1 would explain nothing
  else if (lost) writeSync(2, "kl-tui: lost the bench connection\n");
};
process.on("exit", done);
// The bench went away under us: leave rather than render a dead session.
c.exited.then((code) => {
  lost = code !== 0;
  process.exit(lost ? 1 : 0);
});
await start();
