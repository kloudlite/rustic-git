#!/usr/bin/env bun
// The pod's TUI: a client of the bench daemon (packages/backend/src/daemon.ts), through a relay. The agent lives in the daemon, not here, so closing this terminal (ttyd tab,
// reload) leaves a running turn going and the next TUI finds it busy.
import { fileURLToPath } from "node:url";
import { connect } from "@kloudlite-tui/backend";
import { boot } from "./hello.ts";
import { start } from "./start.tsx";

let c: Awaited<ReturnType<typeof connect>>;
try {
  c = await connect([process.execPath, "run", "--silent", fileURLToPath(new URL("../../../packages/backend/src/relay.ts", import.meta.url))]);
} catch (e: any) {
  process.stderr.write(`kl: ${e.message}\n`);
  process.exit(e.code ?? 1);
}
boot(c.backend, c.hello);
// The daemon went away under us (it restarted): leave rather than render a dead session.
c.exited.then(() => {
  const tail = c.stderr();
  process.stderr.write(tail || "kl: lost the bench backend\n");
  process.exit(1);
});
await start();
