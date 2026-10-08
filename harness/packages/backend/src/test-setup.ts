// Tests must never touch the real ~/.config/kloudlite: named sessions and
// settings written by one test would leak into the next run.
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kloudlite-test-"));
