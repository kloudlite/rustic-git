// Tests must never touch the real ~/.config/kloudlite: named sessions and
// settings written by one test would leak into the next run.
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kloudlite-test-"));
// Boot after the env var: LocalBackend reads settings from KLOUDLITE_CONFIG_DIR.
const { LocalBackend } = await import("@kloudlite-tui/backend/local");
const { boot } = await import("./hello.ts");
const { fixtureSpace } = await import("./fixtures.ts");
const local = new LocalBackend();
// the demo space stands in for the platform: tests have no KL_API_URL, and the sidebar is real data now
local.space = async () => fixtureSpace();
boot(local, await local.hello());
