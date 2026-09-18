import fs from "node:fs";
import crypto from "node:crypto";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  deterministicBaselineSubject,
  loadEvaluationCorpus,
  loadEvaluationOracleBundle,
  runEvaluation,
  unavailableCurrentSubject,
} from "./evaluation.ts";
import type { EvaluationAttempt } from "./evaluation.ts";
import { typeSafeEvaluationSubject } from "./evaluation-subjects.ts";
import type { TypeSafeEvaluationSubjectConfig } from "./evaluation-subjects.ts";
import { validateTypeSafeConfig } from "./typesafe.ts";

type CliOptions = {
  corpus: string;
  oracles: string;
  output: string;
  bootstrap: string;
  pricing?: string;
  runId?: string;
};

export type RunEvaluationCliDependencies = {
  repoRoot?: string;
  clock?: () => number;
  loadBootstrap?: (file: string) => Promise<EvaluationBootstrap>;
  writeStdout?: (line: string) => void;
  writeStderr?: (line: string) => void;
  outputWriter?: (file: string, value: unknown) => void;
  /** Test dependency only. Production bootstrap never sets this. */
  allowCommittedOracleForTests?: true;
  /** Test dependency only. Production bootstrap never sets this. */
  allowBootstrapPathForTests?: true;
};

export type EvaluationBootstrap = {
  /** CI/reviewer injection only; keep this module and reviewer oracles outside committed source/fixture directories. */
  typeSafeConfig?: TypeSafeEvaluationSubjectConfig;
  faultScenarios?: Readonly<Record<string, () => Promise<EvaluationAttempt>>>;
};

const VALUE_FLAGS = new Set(["--corpus", "--oracles", "--output", "--bootstrap", "--pricing", "--run-id"]);
const REQUIRED_FLAGS = ["corpus", "oracles", "output", "bootstrap"] as const;
const COMMITTED_ORACLE_DIR_NAMES = new Set(["src", "test", "fixtures"]);

function parseArgs(args: readonly string[]): CliOptions {
  const parsed: Record<string, string> = {};
  for (let index = 0; index < args.length; index += 2) {
    const flag = args[index];
    const value = args[index + 1];
    if (!VALUE_FLAGS.has(flag ?? "") || value === undefined || value.startsWith("--")) throw new Error("argument_error");
    const key = flag.slice(2).replaceAll("-", "_");
    if (parsed[key] !== undefined) throw new Error("argument_error");
    parsed[key] = value;
  }
  if (REQUIRED_FLAGS.some((key) => !parsed[key])) throw new Error("argument_error");
  return {
    corpus: parsed.corpus,
    oracles: parsed.oracles,
    output: parsed.output,
    bootstrap: parsed.bootstrap,
    ...(parsed.pricing ? { pricing: parsed.pricing } : {}),
    ...(parsed.run_id ? { runId: parsed.run_id } : {}),
  };
}

function isWithin(file: string, directory: string): boolean {
  const relative = path.relative(directory, file);
  return relative === "" || (!relative.startsWith(`..${path.sep}`) && relative !== "..");
}

function assertOracleCustody(file: string, repoRoot: string, allowedForTests: boolean): void {
  if (allowedForTests) return;
  const realFile = fs.realpathSync(file);
  const realRoot = fs.realpathSync(repoRoot);
  if (!isWithin(realFile, realRoot)) return;
  const segments = path.relative(realRoot, realFile).split(path.sep);
  if (segments.some((segment) => COMMITTED_ORACLE_DIR_NAMES.has(segment))) throw new Error("oracle_custody_error");
}

function assertBootstrapCustody(file: string, repoRoot: string, allowedForTests: boolean): void {
  if (allowedForTests) return;
  const realFile = fs.realpathSync(file);
  const realRoot = fs.realpathSync(repoRoot);
  const stat = fs.statSync(realFile);
  if (!stat.isFile() || isWithin(realFile, realRoot)) throw new Error("bootstrap_custody_error");
  if (process.platform !== "win32") {
    if (typeof process.getuid === "function" && stat.uid !== process.getuid()) throw new Error("bootstrap_custody_error");
    if ((stat.mode & 0o022) !== 0) throw new Error("bootstrap_custody_error");
  }
}

function readJson(file: string): unknown {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

async function importBootstrap(file: string): Promise<EvaluationBootstrap> {
  const realFile = fs.realpathSync(file);
  const module = await import(new URL(`file://${realFile}`).href) as { createEvaluationBootstrap?: unknown };
  if (typeof module.createEvaluationBootstrap !== "function") throw new Error("provider_config_unavailable");
  return await (module.createEvaluationBootstrap as () => Promise<EvaluationBootstrap> | EvaluationBootstrap)();
}

export type AtomicFs = Pick<typeof fs, "mkdirSync" | "openSync" | "writeFileSync" | "fsyncSync" | "closeSync" | "renameSync" | "unlinkSync">;

export function atomicWriteJson(file: string, value: unknown, io: AtomicFs = fs, nonce: () => string = crypto.randomUUID): void {
  const directory = path.dirname(path.resolve(file));
  io.mkdirSync(directory, { recursive: true, mode: 0o700 });
  const temporary = path.join(directory, `.${path.basename(file)}.${process.pid}.${nonce()}.tmp`);
  let descriptor: number | undefined;
  try {
    descriptor = io.openSync(temporary, "wx", 0o600);
    io.writeFileSync(descriptor, `${JSON.stringify(value, null, 2)}\n`, "utf8");
    io.fsyncSync(descriptor);
    io.closeSync(descriptor);
    descriptor = undefined;
    io.renameSync(temporary, file);
    try {
      const parent = io.openSync(directory, "r");
      try { io.fsyncSync(parent); } finally { io.closeSync(parent); }
    } catch (error) {
      if (process.platform !== "win32") throw error;
    }
  } finally {
    if (descriptor !== undefined) try { io.closeSync(descriptor); } catch { /* cleanup continues */ }
    try { io.unlinkSync(temporary); } catch (error) {
      if (!(error instanceof Error && "code" in error && error.code === "ENOENT")) throw error;
    }
  }
}

function validateBootstrap(value: unknown, requiredScenarios: ReadonlySet<string>): EvaluationBootstrap | "provider_config_unavailable" | "fault_scenarios_unavailable" | "bootstrap_config_error" {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return "bootstrap_config_error";
  const raw = value as Record<string, unknown>;
  if (Object.keys(raw).some((key) => key !== "typeSafeConfig" && key !== "faultScenarios")) return "bootstrap_config_error";
  if (raw.typeSafeConfig === undefined) return "provider_config_unavailable";
  if (!validateTypeSafeConfig(raw.typeSafeConfig).ok) return "bootstrap_config_error";
  if (raw.faultScenarios === undefined) return requiredScenarios.size === 0 ? { typeSafeConfig: raw.typeSafeConfig as TypeSafeEvaluationSubjectConfig, faultScenarios: {} } : "fault_scenarios_unavailable";
  if (raw.faultScenarios === null || typeof raw.faultScenarios !== "object" || Array.isArray(raw.faultScenarios)) return "bootstrap_config_error";
  const scenarios = raw.faultScenarios as Record<string, unknown>;
  const keys = Object.keys(scenarios);
  if (keys.some((key) => !requiredScenarios.has(key)) || keys.some((key) => typeof scenarios[key] !== "function")) return "bootstrap_config_error";
  if ([...requiredScenarios].some((key) => typeof scenarios[key] !== "function")) return "fault_scenarios_unavailable";
  return { typeSafeConfig: raw.typeSafeConfig as TypeSafeEvaluationSubjectConfig, faultScenarios: scenarios as EvaluationBootstrap["faultScenarios"] };
}

function errorCode(error: unknown, fallback: string): string {
  if (error instanceof Error && /^[a-z_]+$/.test(error.message)) return error.message;
  return fallback;
}

export async function runEvaluationCli(args: readonly string[], deps: RunEvaluationCliDependencies = {}): Promise<number> {
  const stdout = deps.writeStdout ?? console.log;
  const stderr = deps.writeStderr ?? console.error;
  let options: CliOptions;
  try {
    options = parseArgs(args);
  } catch (error) {
    stderr(errorCode(error, "argument_error"));
    return 2;
  }

  const repoRoot = deps.repoRoot ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../..");
  try {
    assertOracleCustody(options.oracles, repoRoot, deps.allowCommittedOracleForTests === true);
  } catch (error) {
    stderr(errorCode(error, "oracle_custody_error"));
    return 3;
  }
  try {
    assertBootstrapCustody(options.bootstrap, repoRoot, deps.allowBootstrapPathForTests === true || deps.loadBootstrap !== undefined);
  } catch {
    stderr("bootstrap_custody_error");
    return 3;
  }

  let corpus;
  let oracles;
  let pricing: unknown;
  try {
    corpus = loadEvaluationCorpus(options.corpus);
  } catch {
    stderr("corpus_error");
    return 4;
  }
  try {
    oracles = loadEvaluationOracleBundle(options.oracles);
  } catch {
    stderr("oracle_error");
    return 5;
  }
  if (options.pricing) try {
    pricing = readJson(options.pricing);
  } catch {
    stderr("pricing_error");
    return 6;
  }

  let bootstrap: EvaluationBootstrap;
  try {
    bootstrap = await (deps.loadBootstrap ?? importBootstrap)(options.bootstrap);
  } catch {
    stderr("provider_bootstrap_error");
    return 7;
  }
  const requiredScenarios = new Set(corpus.cases.flatMap((testCase) => testCase.injectedFault ? [testCase.injectedFault.scenarioId] : []));
  const validatedBootstrap = validateBootstrap(bootstrap, requiredScenarios);
  if (typeof validatedBootstrap === "string") {
    stderr(validatedBootstrap);
    return 7;
  }
  bootstrap = validatedBootstrap;

  let report;
  try {
    report = await runEvaluation(corpus, {
      suite: {
        baseline: deterministicBaselineSubject(),
        current: unavailableCurrentSubject("missing_current_heuristic"),
        proposed: typeSafeEvaluationSubject(bootstrap.typeSafeConfig),
      },
      reviewerOracles: oracles,
      ...(pricing === undefined ? {} : { pricing }),
      ...(options.runId === undefined ? {} : { runId: options.runId }),
      ...(deps.clock === undefined ? {} : { clock: deps.clock }),
      faultScenarios: bootstrap.faultScenarios,
    });
  } catch {
    stderr("suite_error");
    return 8;
  }

  try {
    (deps.outputWriter ?? atomicWriteJson)(options.output, report);
  } catch {
    stderr("output_error");
    return 9;
  }
  stdout(`runId=${report.runId ?? "none"}`);
  stdout(`cases=tuning:${report.splits.tuning},held_out:${report.splits.held_out}`);
  stdout(`output=${path.resolve(options.output)}`);
  const failures = report.subjects.flatMap((subject) => subject.availability === "available" ? Object.entries(subject.totals.failures) : []).reduce<Record<string, number>>((totals, [code, count]) => {
    totals[code] = (totals[code] ?? 0) + count;
    return totals;
  }, {});
  stdout(`totals=failures:${Object.entries(failures).map(([code, count]) => `${code}:${count}`).join(",") || "none"}`);
  return 0;
}

async function main(): Promise<void> {
  process.exitCode = await runEvaluationCli(process.argv.slice(2));
}

if (process.argv[1] && fs.existsSync(process.argv[1]) && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) void main();
