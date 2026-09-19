import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { atomicWriteJson, runEvaluationCli } from "../src/operations/run-evaluation.ts";
import type { AtomicFs } from "../src/operations/run-evaluation.ts";
import type { EvaluationAttempt } from "../src/operations/evaluation.ts";

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtureDir = path.join(here, "fixtures", "operations-evaluation");
const corpusFixture = path.join(fixtureDir, "corpus.json");
const oracleFixture = path.join(fixtureDir, "reviewer-oracles.json");
const fixedClock = () => 1_700_000_000_000;
const faultScenarios: Record<string, () => Promise<EvaluationAttempt>> = {
  f001: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }),
  f002: async () => ({ outcome: "provider_failure", failure: { code: "invalid_response" } }),
};
const testTypeSafeConfig = {
  apiKey: "test-key-never-dispatched",
  providerInputPolicy: (request: { digest: string }) => ({ authorized: true as const, digest: request.digest }),
  fetch: async () => { throw new Error("network disabled"); },
  maxAttempts: 1,
};

function temp(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "operation-evaluation-"));
}

function resolveBunExecutable(): string | undefined {
  if (process.env.BUN_EXE) return process.env.BUN_EXE;
  if (process.versions.bun) return process.execPath;
  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    const candidate = path.join(directory, process.platform === "win32" ? "bun.exe" : "bun");
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // Keep searching PATH.
    }
  }
  return undefined;
}

function capture() {
  const stdout: string[] = [];
  const stderr: string[] = [];
  return { stdout, stderr, writeStdout: (line: string) => stdout.push(line), writeStderr: (line: string) => stderr.push(line) };
}

function args(root: string, oracle = path.join(root, "reviewer-oracles.json")): string[] {
  fs.copyFileSync(oracleFixture, oracle);
  return ["--corpus", corpusFixture, "--oracles", oracle, "--output", path.join(root, "reports", "report.json"), "--bootstrap", path.join(root, "bootstrap.mjs"), "--run-id", "test-run"];
}

const bootstrap = { typeSafeConfig: testTypeSafeConfig, faultScenarios };

test("requires explicit corpus, oracles, and output", async () => {
  for (const argv of [[], ["--corpus", corpusFixture], ["--corpus", corpusFixture, "--oracles", oracleFixture]]) {
    const io = capture();
    assert.notEqual(await runEvaluationCli(argv, { ...io, repoRoot: path.resolve(here, "../../..") }), 0);
    assert.deepEqual(io.stderr, ["argument_error"]);
  }
});

test("refuses committed and symlinked reviewer fixtures without a test dependency", async () => {
  const root = temp();
  const repoRoot = path.resolve(here, "../../..");
  for (const oracle of [oracleFixture, path.join(root, "oracle-link.json")]) {
    if (oracle.endsWith("oracle-link.json")) fs.symlinkSync(oracleFixture, oracle);
    const io = capture();
    const exit = await runEvaluationCli(["--corpus", corpusFixture, "--oracles", oracle, "--output", path.join(root, "report.json"), "--bootstrap", path.join(root, "bootstrap.mjs")], { ...io, repoRoot });
    assert.notEqual(exit, 0);
    assert.deepEqual(io.stderr, ["oracle_custody_error"]);
  }
});

test("refuses reviewer paths in any committed source or fixture directory", async () => {
  const root = temp();
  const repoRoot = path.join(root, "repo");
  const oracle = path.join(repoRoot, "other", "fixtures", "reviewer.json");
  fs.mkdirSync(path.dirname(oracle), { recursive: true });
  fs.copyFileSync(oracleFixture, oracle);
  const io = capture();
  const exit = await runEvaluationCli(["--corpus", corpusFixture, "--oracles", oracle, "--output", path.join(root, "report.json"), "--bootstrap", path.join(root, "bootstrap.mjs")], { ...io, repoRoot });
  assert.notEqual(exit, 0);
  assert.deepEqual(io.stderr, ["oracle_custody_error"]);
});

test("bootstrap custody rejects repository paths, symlinks into the repository, and writable files", async () => {
  const root = temp();
  const repoRoot = path.resolve(here, "../../..");
  const outsideOracle = path.join(root, "reviewer-oracles.json");
  fs.copyFileSync(oracleFixture, outsideOracle);
  const committed = path.join(here, "fixtures", "operations-evaluation", "bootstrap.mjs");
  const symlink = path.join(root, "bootstrap-link.mjs");
  fs.writeFileSync(committed, "export function createEvaluationBootstrap() {}\n");
  fs.symlinkSync(committed, symlink);
  const writable = path.join(root, "writable-bootstrap.mjs");
  fs.writeFileSync(writable, "export function createEvaluationBootstrap() {}\n", { mode: 0o600 });
  fs.chmodSync(writable, 0o666);
  try {
    for (const bootstrapFile of [committed, symlink, writable]) {
      const io = capture();
      const exit = await runEvaluationCli(["--corpus", corpusFixture, "--oracles", outsideOracle, "--output", path.join(root, "report.json"), "--bootstrap", bootstrapFile], { ...io, repoRoot });
      assert.notEqual(exit, 0);
      assert.deepEqual(io.stderr, ["bootstrap_custody_error"]);
    }
  } finally {
    fs.unlinkSync(committed);
  }
});

test("test-only bootstrap custody dependency permits an injected repository path", async () => {
  const root = temp();
  const io = capture();
  const exit = await runEvaluationCli(args(root), {
    ...io,
    repoRoot: path.resolve(here, "../../.."),
    allowBootstrapPathForTests: true,
    loadBootstrap: async () => bootstrap,
  });
  assert.equal(exit, 0);
});

test("test-only custody dependency permits the fixture and joins reviewer oracles", async () => {
  const root = temp();
  const io = capture();
  const exit = await runEvaluationCli(["--corpus", corpusFixture, "--oracles", oracleFixture, "--output", path.join(root, "report.json"), "--bootstrap", path.join(root, "bootstrap.mjs")], {
    ...io,
    repoRoot: path.resolve(here, "../../.."),
    allowCommittedOracleForTests: true,
    clock: fixedClock,
    loadBootstrap: async () => bootstrap,
  });
  assert.equal(exit, 0);
  const report = JSON.parse(fs.readFileSync(path.join(root, "report.json"), "utf8"));
  assert.ok(report.splits.held_out > 0);
  assert.equal(report.cases.some((entry: { split: string }) => entry.split === "held_out"), true);
});

test("writes deterministic report atomically with unavailable current", async () => {
  const first = temp();
  const second = temp();
  const run = async (root: string) => {
    const io = capture();
    const argv = args(root);
    const exit = await runEvaluationCli(argv, { ...io, repoRoot: path.resolve(here, "../../.."), clock: fixedClock, loadBootstrap: async () => bootstrap });
    assert.equal(exit, 0);
    assert.deepEqual(io.stderr, []);
    assert.equal(io.stdout.some((line) => line.includes("apiKey") || line.includes("secret")), false);
    assert.equal(fs.readdirSync(path.join(root, "reports")).some((name) => name.includes(".tmp")), false);
    return fs.readFileSync(path.join(root, "reports", "report.json"), "utf8");
  };
  const left = await run(first);
  const right = await run(second);
  assert.equal(left, right);
  const report = JSON.parse(left);
  assert.equal(report.runId, "test-run");
  assert.deepEqual(report.subjects.find((subject: { role: string }) => subject.role === "current"), {
    subjectId: "current-unavailable",
    role: "current",
    availability: "unavailable",
    reason: "missing_current_heuristic",
    cohortFingerprint: report.cohortFingerprint,
  });
  const proposed = report.subjects.find((subject: { role: string }) => subject.role === "proposed");
  assert.ok(proposed);
  assert.equal(proposed.availability, "available");
});

test("invalid corpus and provider bootstrap errors are nonzero and sanitized", async () => {
  const root = temp();
  const argv = args(root);
  const badCorpus = path.join(root, "bad-corpus.json");
  fs.writeFileSync(badCorpus, "{}\n");
  const corpusIo = capture();
  assert.notEqual(await runEvaluationCli(["--corpus", badCorpus, ...argv.slice(2)], { ...corpusIo, repoRoot: path.resolve(here, "../../.."), loadBootstrap: async () => bootstrap }), 0);
  assert.deepEqual(corpusIo.stderr, ["corpus_error"]);

  const secret = "sk-never-print-this-secret";
  const providerIo = capture();
  assert.notEqual(await runEvaluationCli(argv, {
    ...providerIo,
    repoRoot: path.resolve(here, "../../.."),
    loadBootstrap: async () => { throw new Error(secret); },
  }), 0);
  assert.deepEqual(providerIo.stderr, ["provider_bootstrap_error"]);
  assert.equal([...providerIo.stdout, ...providerIo.stderr].join("\n").includes(secret), false);
});

test("injected provider config can run without live network", async () => {
  const root = temp();
  const io = capture();
  let calls = 0;
  const exit = await runEvaluationCli(args(root), {
    ...io,
    repoRoot: path.resolve(here, "../../.."),
    clock: fixedClock,
    loadBootstrap: async () => ({
      faultScenarios,
      typeSafeConfig: {
        apiKey: "test-key-that-is-never-printed",
        providerInputPolicy: (request) => ({ authorized: true, digest: request.digest }),
        fetch: async () => {
          calls += 1;
          throw new Error("network disabled");
        },
        maxAttempts: 1,
      },
    }),
  });
  assert.equal(exit, 0);
  assert.equal(calls > 0, true);
  assert.equal([...io.stdout, ...io.stderr].join("\n").includes("test-key"), false);
});

test("missing or incomplete trusted bootstrap fails before writing a report", async () => {
  const root = temp();
  const argv = args(root);
  for (const [loadBootstrap, code] of [
    [async () => ({ faultScenarios }), "provider_config_unavailable"],
    [async () => ({ typeSafeConfig: testTypeSafeConfig }), "fault_scenarios_unavailable"],
  ] as const) {
    const io = capture();
    assert.notEqual(await runEvaluationCli(argv, { ...io, repoRoot: path.resolve(here, "../../.."), loadBootstrap }), 0);
    assert.deepEqual(io.stderr, [code]);
    assert.equal(fs.existsSync(path.join(root, "reports", "report.json")), false);
  }
});

test("rejects malformed provider config and fault scenario mappings before attempts", async () => {
  const root = temp();
  const argv = args(root);
  const cases = [
    [{ typeSafeConfig: { apiKey: "Bearer fake-secret", unknown: "transport-body" }, faultScenarios }, "bootstrap_config_error"],
    [{ typeSafeConfig: testTypeSafeConfig, faultScenarios: { f001: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }) } }, "fault_scenarios_unavailable"],
    [{ typeSafeConfig: testTypeSafeConfig, faultScenarios: { ...faultScenarios, extra: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }) } }, "bootstrap_config_error"],
    [{ typeSafeConfig: testTypeSafeConfig, faultScenarios: { f001: "not-function", f002: faultScenarios.f002 } }, "bootstrap_config_error"],
  ] as const;
  for (const [candidate, code] of cases) {
    const io = capture();
    const exit = await runEvaluationCli(argv, {
      ...io,
      repoRoot: path.resolve(here, "../../.."),
      loadBootstrap: async () => candidate as never,
    });
    assert.notEqual(exit, 0);
    assert.deepEqual(io.stderr, [code]);
    assert.equal([...io.stdout, ...io.stderr].join("\n").includes("fake-secret"), false);
    assert.equal([...io.stdout, ...io.stderr].join("\n").includes("transport-body"), false);
  }
});

test("oracle and pricing errors expose only stable codes", async () => {
  const root = temp();
  const argv = args(root);
  const secret = "Bearer fake-reviewer-secret";
  const badOracle = path.join(root, "bad-oracle.json");
  fs.writeFileSync(badOracle, JSON.stringify({ secret }));
  const badPricing = path.join(root, "bad-pricing.json");
  fs.writeFileSync(badPricing, `{${secret}`);
  for (const [extraArgs, code] of [
    [["--corpus", corpusFixture, "--oracles", badOracle, "--output", path.join(root, "a.json"), "--bootstrap", argv[7]], "oracle_error"],
    [[...argv, "--pricing", badPricing], "pricing_error"],
  ] as const) {
    const io = capture();
    assert.notEqual(await runEvaluationCli(extraArgs, { ...io, repoRoot: path.resolve(here, "../../.."), loadBootstrap: async () => bootstrap }), 0);
    assert.deepEqual(io.stderr, [code]);
    assert.equal([...io.stdout, ...io.stderr].join("\n").includes(secret), false);
  }
});

test("output failure preserves the prior report and removes temporary files", async () => {
  const root = temp();
  const argv = args(root);
  const output = path.join(root, "reports", "report.json");
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, "prior-report\n", { mode: 0o600 });
  const io = capture();
  const exit = await runEvaluationCli(argv, {
    ...io,
    repoRoot: path.resolve(here, "../../.."),
    loadBootstrap: async () => bootstrap,
    outputWriter: () => { throw new Error("Bearer fake-output-secret"); },
  });
  assert.notEqual(exit, 0);
  assert.deepEqual(io.stderr, ["output_error"]);
  assert.equal(fs.readFileSync(output, "utf8"), "prior-report\n");
  assert.equal(fs.readdirSync(path.dirname(output)).some((name) => name.includes(".tmp")), false);
  assert.equal([...io.stdout, ...io.stderr].join("\n").includes("fake-output-secret"), false);
});

function atomicFs(failAt?: "file_sync" | "rename"): { api: AtomicFs; calls: Array<readonly unknown[]> } {
  const calls: Array<readonly unknown[]> = [];
  let nextFd = 10;
  const api: AtomicFs = {
    mkdirSync: (file, options) => { calls.push(["mkdir", file, options]); return undefined; },
    openSync: (file, flags, mode) => { calls.push(["open", file, flags, ...(mode === undefined ? [] : [mode])]); return nextFd++; },
    writeFileSync: (file, data, options) => { calls.push(["write", file, data, options]); },
    fsyncSync: (fd: number) => {
      calls.push(["sync", fd]);
      if (failAt === "file_sync" && fd === 10) throw new Error("file sync failed");
    },
    closeSync: (fd) => { calls.push(["close", fd]); },
    renameSync: (oldPath, newPath) => {
      calls.push(["rename", oldPath, newPath]);
      if (failAt === "rename") throw new Error("rename failed");
    },
    unlinkSync: (file) => { calls.push(["unlink", file]); },
  };
  return { api, calls };
}

test("atomic writer uses owner-only exclusive temp, file sync, rename, and parent sync", () => {
  const fake = atomicFs();
  atomicWriteJson("/reports/report.json", { ok: true }, fake.api, () => "nonce");
  const temporary = String(fake.calls.find(([call]) => call === "open")?.[1]);
  assert.match(temporary, /^\/reports\/\.report\.json\.[0-9]+\.nonce\.tmp$/);
  assert.deepEqual(fake.calls, [
    ["mkdir", "/reports", { recursive: true, mode: 0o700 }],
    ["open", temporary, "wx", 0o600],
    ["write", 10, '{\n  "ok": true\n}\n', "utf8"],
    ["sync", 10],
    ["close", 10],
    ["rename", temporary, "/reports/report.json"],
    ["open", "/reports", "r"],
    ["sync", 11],
    ["close", 11],
    ["unlink", temporary],
  ]);
});

test("atomic writer cleans temp and never renames when file sync fails", () => {
  const fake = atomicFs("file_sync");
  assert.throws(() => atomicWriteJson("/reports/report.json", { replacement: true }, fake.api, () => "sync-fail"), /file sync failed/);
  assert.equal(fake.calls.some(([call]) => call === "rename"), false);
  assert.equal(fake.calls.some(([call, file]) => call === "unlink" && String(file).includes("sync-fail")), true);
});

test("atomic writer cleans temp and leaves destination untouched when rename fails", () => {
  const fake = atomicFs("rename");
  assert.throws(() => atomicWriteJson("/reports/report.json", { replacement: true }, fake.api, () => "rename-fail"), /rename failed/);
  assert.equal(fake.calls.filter(([call]) => call === "rename").length, 1);
  assert.equal(fake.calls.some(([call, file]) => call === "unlink" && String(file).includes("rename-fail")), true);
  assert.equal(fake.calls.filter(([call]) => call === "write").length, 1, "the destination is never opened or written directly");
});

test("missing trusted bootstrap module returns a stable error", async () => {
  const root = temp();
  const io = capture();
  const argv = args(root);
  assert.notEqual(await runEvaluationCli(argv, { ...io, repoRoot: path.resolve(here, "../../..") }), 0);
  assert.deepEqual(io.stderr, ["bootstrap_custody_error"]);
});

test("spawned CLI loads an external trusted bootstrap and emits compact stdout", async () => {
  const root = temp();
  const oracle = path.join(root, "reviewer-oracles.json");
  fs.copyFileSync(oracleFixture, oracle);
  const bootstrapFile = path.join(root, "bootstrap.mjs");
  fs.writeFileSync(bootstrapFile, `export async function createEvaluationBootstrap() { return {\n  typeSafeConfig: { apiKey: "spawn-test-key", providerInputPolicy: request => ({ authorized: true, digest: request.digest }), fetch: async () => { throw new Error("offline") }, maxAttempts: 1 },\n  faultScenarios: { f001: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }), f002: async () => ({ outcome: "provider_failure", failure: { code: "invalid_response" } }) }\n}; }\n`);
  const output = path.join(root, "report.json");
  const result = spawnSync(process.execPath, [fileURLToPath(new URL("../src/operations/run-evaluation.ts", import.meta.url)), "--corpus", corpusFixture, "--oracles", oracle, "--output", output, "--bootstrap", bootstrapFile, "--run-id", "spawn-run"], { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(output), true);
  assert.deepEqual(result.stdout.trim().split("\n").map((line) => line.split("=")[0]), ["runId", "cases", "output", "totals"]);
  assert.equal(result.stdout.includes("wholeCall"), false);
  assert.equal(result.stdout.includes("spawn-test-key"), false);
});

test("package script preserves the executable argument boundary", async () => {
  const bun = resolveBunExecutable();
  assert.ok(bun, "bun executable required");
  const root = temp();
  const oracle = path.join(root, "reviewer-oracles.json");
  fs.copyFileSync(oracleFixture, oracle);
  const bootstrapFile = path.join(root, "bootstrap.mjs");
  fs.writeFileSync(bootstrapFile, `export function createEvaluationBootstrap() { return { typeSafeConfig: { apiKey: "package-test-key", providerInputPolicy: request => ({ authorized: true, digest: request.digest }), fetch: async () => { throw new Error("offline") }, maxAttempts: 1 }, faultScenarios: { f001: async () => ({ outcome: "provider_failure", failure: { code: "timeout" } }), f002: async () => ({ outcome: "provider_failure", failure: { code: "invalid_response" } }) } }; }\n`);
  const output = path.join(root, "package-report.json");
  const harnessRoot = path.resolve(here, "../..");
  const result = spawnSync(bun, ["run", "evaluation:operations", "--", "--corpus", corpusFixture, "--oracles", oracle, "--output", output, "--bootstrap", bootstrapFile], { cwd: harnessRoot, encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.existsSync(output), true);
  assert.equal(result.stdout.includes("package-test-key"), false);
});
