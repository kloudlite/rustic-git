// Tests must never touch the real ~/.config/kloudlite: named sessions and
// settings written by one test would leak into the next run.
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kloudlite-test-"));
// Boot after the env var: LocalBackend reads settings from KLOUDLITE_CONFIG_DIR.
const { LocalBackend } = await import("@kloudlite-tui/backend/local");
const { boot } = await import("./hello.ts");
const local = new LocalBackend();
boot(local, await local.hello());
