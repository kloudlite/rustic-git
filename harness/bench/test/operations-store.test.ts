import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  ContractViolation,
  canonicalDigest,
  deriveDeduplicationKey,
  validateCompactOperationResult,
  validateOperationSnapshot,
  type OperateRequest,
  type OperationSnapshot,
  type RecordedDecision,
  type ResumeRequest,
  type TrustedActorContext,
} from "../src/operations/contracts.ts";
import {
  encodeCommitFrame,
  DuplicateRequestConflictError,
  InvalidOperationIdError,
  OperationLogCorruptError,
  OperationStore,
  OperationStoreError,
  StoreClosedError,
  StoreNotOwnedError,
  nodeStoreFs,
  type ApprovalRequirement,
  type CapabilityMetadata,
  type RetryPolicy,
  type RetryStepInput,
  type StoreFs,
} from "../src/operations/store.ts";
import { DispatchAuthority } from "../src/operations/dispatch-authority.ts";

const FIXTURES = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures", "operations-o05");
const INSTRUCTION = "In src/config.ts, change the timeout to 30000.";
const PAYLOAD_A = canonicalDigest({ path: "src/config.ts", revision: 4 });
const PAYLOAD_B = canonicalDigest({ path: "src/config.ts", revision: 5 });

/** Test-owned bench folder fence; `release` stands in for another owner taking over. */
class OwnershipStub {
  readonly ownerId = "test-bench-owner";
  held = true;
  assertHeld(): void {
    if (!this.held) throw new StoreNotOwnedError("the folder lock was released");
  }
  release(): void {
    this.held = false;
  }
}

function clockFrom(start = 1_760_000_000_000) {
  let now = start;
  return { now: () => now, advance: (ms: number) => void (now += ms) };
}

function tempRoot(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "bench-opstore-"));
}

function context(over: Partial<TrustedActorContext> = {}): TrustedActorContext {
  return {
    actorId: "user-1",
    tenantId: "tenant-1",
    sessionId: "s-1",
    turnId: "turn-1",
    toolCallId: "call-1",
    turnRevision: 1,
    scope: { workspaceId: "ws-api" },
    ...over,
  };
}

function instruction(text = INSTRUCTION): OperateRequest {
  return { instruction: text };
}

type Options = {
  root?: string;
  clock?: ReturnType<typeof clockFrom>;
  ownership?: OwnershipStub;
  fs?: StoreFs;
  capabilityMetadata?: (capability: string, version: string) => CapabilityMetadata | undefined;
  generateOperationId?: () => string;
  dispatchAuthority?: DispatchAuthority;
};

function openStore(options: Options = {}) {
  const root = options.root ?? tempRoot();
  const clock = options.clock ?? clockFrom();
  const ownership = options.ownership ?? new OwnershipStub();
  const store = new OperationStore({
    root,
    ownership,
    now: clock.now,
    ...(options.fs ? { fs: options.fs } : {}),
    capabilityMetadata: options.capabilityMetadata ?? testMetadata,
    dispatchAuthority: options.dispatchAuthority ?? new DispatchAuthority(),
    ...(options.generateOperationId ? { generateOperationId: options.generateOperationId } : {}),
  });
  return { root, clock, ownership, store };
}

function reopened(root: string, clock: ReturnType<typeof clockFrom>, options: Options = {}) {
  return new OperationStore({
    root,
    ownership: new OwnershipStub(),
    now: clock.now,
    ...(options.fs ? { fs: options.fs } : {}),
    ...(options.generateOperationId ? { generateOperationId: options.generateOperationId } : {}),
    capabilityMetadata: options.capabilityMetadata ?? testMetadata,
    dispatchAuthority: options.dispatchAuthority ?? new DispatchAuthority(),
  });
}

function testMetadata(capability: string): CapabilityMetadata | undefined {
  if (capability === "file.read") return { version: "1.0.0", effect: "read", approval: "none", retry: { class: "none", maxAttempts: 1 } };
  if (capability === "file.edit") {
    return {
      version: "1.0.0",
      effect: "write",
      resourceKeys: ["workspace.file:src/config.ts"],
      approval: "user",
      retry: { class: "none", maxAttempts: 1 },
    };
  }
  return undefined;
}

function logFile(root: string, operationId: string): string {
  return path.join(root, "operations", `${operationId}.jsonl`);
}

/** Logs the durable call order so a test can assert fsync ordering. */
function instrumentedFs(log: string[]): StoreFs {
  const targets = new Map<number, string>();
  const kind = (target: string) => (target.endsWith(".jsonl") ? "file" : "dir");
  return {
    mkdirSync: (target) => {
      log.push("mkdir:dir");
      nodeStoreFs.mkdirSync(target);
    },
    readdirSync: (target) => nodeStoreFs.readdirSync(target),
    existsSync: (target) => nodeStoreFs.existsSync(target),
    readFileSync: (target) => nodeStoreFs.readFileSync(target),
    openSync: (target, flags) => {
      const fd = nodeStoreFs.openSync(target, flags);
      targets.set(fd, target);
      log.push(`open:${flags}:${kind(target)}`);
      return fd;
    },
    writeSync: (fd, data) => {
      const written = nodeStoreFs.writeSync(fd, data);
      log.push(`write:${kind(targets.get(fd) ?? "")}`);
      return written;
    },
    fsyncSync: (fd) => {
      log.push(`fsync:${kind(targets.get(fd) ?? "")}`);
      nodeStoreFs.fsyncSync(fd);
    },
    closeSync: (fd) => {
      nodeStoreFs.closeSync(fd);
      targets.delete(fd);
    },
    truncateSync: (target, length) => nodeStoreFs.truncateSync(target, length),
    unlinkSync: (target) => nodeStoreFs.unlinkSync(target),
    renameSync: (from, to) => nodeStoreFs.renameSync(from, to),
    fstatSync: (fd) => nodeStoreFs.fstatSync(fd),
    lstatSync: (target) => nodeStoreFs.lstatSync(target),
    chmodSync: (target, mode) => nodeStoreFs.chmodSync(target, mode),
    readSync: (fd, buffer, offset, length, position) => nodeStoreFs.readSync(fd, buffer, offset, length, position),
  };
}

/** Crashes at one durable step, the way a power loss would. */
function crashingFs(mode: "partial-write" | "fsync-after-write"): { fs: StoreFs; crashed: () => boolean } {
  const targets = new Map<number, string>();
  let armed = true;
  let crashed = false;
  const isFile = (fd: number) => (targets.get(fd) ?? "").endsWith(".jsonl");
  return {
    crashed: () => crashed,
    fs: {
      mkdirSync: (target) => nodeStoreFs.mkdirSync(target),
      readdirSync: (target) => nodeStoreFs.readdirSync(target),
      existsSync: (target) => nodeStoreFs.existsSync(target),
      readFileSync: (target) => nodeStoreFs.readFileSync(target),
      openSync: (target, flags) => {
        const fd = nodeStoreFs.openSync(target, flags);
        targets.set(fd, target);
        return fd;
      },
      writeSync: (fd, data) => {
        if (mode === "partial-write" && armed && isFile(fd)) {
          armed = false;
          crashed = true;
          nodeStoreFs.writeSync(fd, data.slice(0, Math.floor(data.length / 2)));
          throw new Error("simulated crash during append");
        }
        return nodeStoreFs.writeSync(fd, data);
      },
      fsyncSync: (fd) => {
        if (mode === "fsync-after-write" && armed && isFile(fd)) {
          armed = false;
          crashed = true;
          throw new Error("simulated crash before fsync");
        }
        nodeStoreFs.fsyncSync(fd);
      },
      closeSync: (fd) => {
        nodeStoreFs.closeSync(fd);
        targets.delete(fd);
      },
      truncateSync: (target, length) => nodeStoreFs.truncateSync(target, length),
      unlinkSync: (target) => nodeStoreFs.unlinkSync(target),
      renameSync: (from, to) => nodeStoreFs.renameSync(from, to),
      fstatSync: (fd) => nodeStoreFs.fstatSync(fd),
      lstatSync: (target) => nodeStoreFs.lstatSync(target),
      chmodSync: (target, mode) => nodeStoreFs.chmodSync(target, mode),
      readSync: (fd, buffer, offset, length, position) => nodeStoreFs.readSync(fd, buffer, offset, length, position),
    },
  };
}

function failingPruneFs(root: string, mode: "unlink" | "directory-fsync"): StoreFs {
  const targets = new Map<number, string>();
  let armed = true;
  let unlinked = false;
  return {
    ...nodeStoreFs,
    openSync: (target, flags, openMode) => {
      const fd = nodeStoreFs.openSync(target, flags, openMode);
      targets.set(fd, target);
      return fd;
    },
    unlinkSync: (target) => {
      if (armed && mode === "unlink" && target.endsWith(".jsonl")) {
        armed = false;
        throw new Error("simulated terminal log unlink failure");
      }
      nodeStoreFs.unlinkSync(target);
      if (target.endsWith(".jsonl")) unlinked = true;
    },
    fsyncSync: (fd) => {
      if (armed && mode === "directory-fsync" && unlinked && targets.get(fd) === path.join(root, "operations")) {
        armed = false;
        throw new Error("simulated prune directory fsync failure");
      }
      nodeStoreFs.fsyncSync(fd);
    },
    closeSync: (fd) => {
      nodeStoreFs.closeSync(fd);
      targets.delete(fd);
    },
  };
}

function isStoreError(error: unknown, code: string): boolean {
  return error instanceof OperationStoreError && error.code === code;
}

function queueEdit(store: OperationStore, operationId: string, key = "edit_config"): OperationSnapshot {
  return store.queueStep(operationId, {
    key,
    capability: "file.edit",
    targetRef: "ws-api",
  });
}

function recordedDecision(
  snapshot: OperationSnapshot,
  over: Partial<RecordedDecision> & { decisionId: string; payloadDigest: string },
): RecordedDecision {
  const pending = snapshot.pendingDecisions.find((entry) => entry.decisionId === over.decisionId);
  assert.ok(pending, `no pending decision ${over.decisionId}`);
  const base: RecordedDecision = {
    recordId: "rec-1",
    operationId: snapshot.operationId,
    stepId: pending.stepId,
    decisionId: over.decisionId,
    decisionClass: "user_authorization",
    actorId: snapshot.actor.actorId,
    tenantId: snapshot.actor.tenantId,
    sessionId: snapshot.actor.sessionId,
    payloadDigest: over.payloadDigest,
    revision: snapshot.revision,
    policySource: "user_ui",
    outcome: "granted",
    recordedAt: pending.createdAt,
    expiresAt: pending.expiresAt,
  };
  return { ...base, ...over };
}

function resumeRequest(operationId: string, decisionId: string, expectedRevision: number, recordId: string): ResumeRequest {
  return { action: "resume", operationId, decisionId, expectedRevision, resolution: { kind: "recorded_user_decision", recordId } };
}

function fixtureRecords(fixture: { operationId: string; records: unknown[]; tornFragment?: string }): string {
  const lines = fixture.records.map((record) => (typeof record === "string" ? record : JSON.stringify(record)));
  return `${lines.join("\n")}\n${fixture.tornFragment ?? ""}`;
}

function legacyRecords(root: string, operationId: string): string {
  const framed = fs.readFileSync(logFile(root, operationId));
  const records: unknown[] = [];
  let offset = Buffer.byteLength("O05v2\n");
  while (offset < framed.length) {
    const newline = framed.indexOf(0x0a, offset);
    const length = Number(framed.subarray(offset, newline).toString("ascii").split(" ", 1)[0]);
    const start = newline + 1;
    records.push(JSON.parse(framed.subarray(start, start + length).toString("utf8")));
    offset = start + length;
  }
  return `${records.map((record) => JSON.stringify(record)).join("\n")}\n`;
}

function readStoredRecords(file: string): any[] {
  const buffer = fs.readFileSync(file);
  if (buffer.subarray(0, 6).toString() !== "O05v2\n") return buffer.toString("utf8").trimEnd().split("\n").map((line) => JSON.parse(line));
  const records: any[] = [];
  let offset = 6;
  while (offset < buffer.length) {
    const newline = buffer.indexOf(0x0a, offset);
    const length = Number(buffer.subarray(offset, newline).toString("ascii").split(" ", 1)[0]);
    const start = newline + 1;
    records.push(JSON.parse(buffer.subarray(start, start + length).toString("utf8")));
    offset = start + length;
  }
  return records;
}

function appendStoredRecord(file: string, record: unknown): void {
  fs.appendFileSync(file, encodeCommitFrame(record as never));
}

function installFixture(root: string, name: string): { operationId: string; file: string } {
  const fixture = JSON.parse(fs.readFileSync(path.join(FIXTURES, name), "utf8")) as {
    operationId: string;
    records: unknown[];
    tornFragment?: string;
  };
  const file = logFile(root, fixture.operationId);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, fixtureRecords(fixture));
  return { operationId: fixture.operationId, file };
}

function tombstoneFixture(root: string, name: string): string {
  const seed = openStore({ root, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = seed.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(seed.store, operationId);
  seed.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  seed.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  seed.store.removeTerminalOperation(operationId);
  const file = path.join(root, "operations", "tombstones.log");
  const complete = fs.readFileSync(file);
  if (name === "tombstone-incomplete-final.bin") fs.writeFileSync(file, Buffer.concat([complete, Buffer.from("100 sha256:")]));
  if (name === "tombstone-bad-checksum.bin") {
    const corrupt = Buffer.from(complete);
    const digest = corrupt.indexOf(Buffer.from("sha256:"));
    corrupt[digest + 8] = corrupt[digest + 8] === 0x30 ? 0x31 : 0x30;
    fs.writeFileSync(file, corrupt);
  }
  return file;
}

test("the O05 fixture manifest names every fixture", () => {
  const manifest = JSON.parse(fs.readFileSync(path.join(FIXTURES, "manifest.json"), "utf8")) as { fixtures: Array<{ file: string; kind: string }> };
  const actual = fs.readdirSync(FIXTURES).filter((name) => name !== "manifest.json").sort();
  assert.deepEqual(manifest.fixtures.map((fixture) => fixture.file).sort(), actual);
  assert.equal(manifest.fixtures.every((fixture) => ["legacy-v1", "v2-log", "tombstones"].includes(fixture.kind)), true);
});

test("every manifested fixture is consumed by its storage reader", () => {
  const manifest = JSON.parse(fs.readFileSync(path.join(FIXTURES, "manifest.json"), "utf8")) as { fixtures: Array<{ file: string; kind: string }> };
  for (const fixture of manifest.fixtures) {
    const root = tempRoot();
    const source = path.join(FIXTURES, fixture.file);
    if (fixture.kind === "tombstones") {
      const destination = path.join(root, "operations", "tombstones.log");
      fs.mkdirSync(path.dirname(destination), { recursive: true });
      fs.writeFileSync(destination, fs.readFileSync(source));
      fs.chmodSync(destination, 0o600);
      if (fixture.file.includes("bad-checksum")) assert.throws(() => reopened(root, clockFrom()), OperationLogCorruptError);
      else if (fixture.file.includes("incomplete-final")) assert.equal(reopened(root, clockFrom()).tornTails.includes(destination), true);
      else assert.doesNotThrow(() => reopened(root, clockFrom()));
    } else {
      let operationId: string;
      if (fixture.kind === "v2-log") {
        operationId = "op-v2-fixture";
        const destination = logFile(root, operationId);
        fs.mkdirSync(path.dirname(destination), { recursive: true });
        fs.writeFileSync(destination, fs.readFileSync(source));
        fs.chmodSync(destination, 0o600);
      } else {
        operationId = installFixture(root, fixture.file).operationId;
      }
      if (fixture.file.includes("interior-corruption")) assert.throws(() => reopened(root, clockFrom()), OperationLogCorruptError);
      else assert.doesNotThrow(() => reopened(root, clockFrom()).load(operationId));
    }
  }
});

test("an incomplete final tombstone frame is repaired before the next durable append", () => {
  const root = tempRoot();
  const file = tombstoneFixture(root, "tombstone-incomplete-final.bin");
  const store = reopened(root, clockFrom(), { generateOperationId: () => "op-after-torn-tombstone" });
  assert.equal(store.tornTails.includes(file), true);
  const accepted = store.accept({ request: instruction(), context: context({ toolCallId: "after-torn" }) });
  queueEdit(store, accepted.snapshot.operationId);
  store.requestCancel(accepted.snapshot.operationId);
  store.removeTerminalOperation(accepted.snapshot.operationId);
  assert.equal(store.repairs.some((repair) => repair.file === file && repair.action === "truncated"), true);
  assert.doesNotThrow(() => reopened(root, clockFrom()));
});

test("a complete corrupt tombstone frame is never truncated", () => {
  const root = tempRoot();
  const file = tombstoneFixture(root, "tombstone-bad-checksum.bin");
  const before = fs.readFileSync(file);
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /checksum/i.test(error.message));
  assert.deepEqual(fs.readFileSync(file), before);
});

test("tombstones reject links and non-regular files and repair restrictive modes", () => {
  const root = tempRoot();
  const operations = path.join(root, "operations");
  fs.mkdirSync(operations, { mode: 0o700 });
  const outside = path.join(root, "outside");
  fs.writeFileSync(outside, "outside");
  fs.symlinkSync(outside, path.join(operations, "tombstones.log"));
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /symbolic link|regular file/i.test(error.message));

  fs.unlinkSync(path.join(operations, "tombstones.log"));
  fs.mkdirSync(path.join(operations, "tombstones.log"));
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /regular file/i.test(error.message));

  fs.rmSync(path.join(operations, "tombstones.log"), { recursive: true });
  const tombstones = tombstoneFixture(root, "tombstone-valid.bin");
  fs.chmodSync(tombstones, 0o644);
  reopened(root, clockFrom());
  assert.equal(fs.statSync(tombstones).mode & 0o777, 0o600);
});

test("v2 framing distinguishes incomplete bytes from complete corruption", () => {
  const { store, root, clock } = openStore({ generateOperationId: () => "op-framed" });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  const file = logFile(root, operationId);
  const complete = fs.readFileSync(file);
  assert.equal(complete.subarray(0, 6).toString(), "O05v2\n");

  fs.writeFileSync(file, complete.subarray(0, complete.length - 3));
  const torn = reopened(root, clock);
  assert.deepEqual(torn.operationIds(), []);
  assert.deepEqual(torn.tornTails, [file]);

  fs.writeFileSync(file, complete);
  const digestOffset = complete.indexOf(Buffer.from("sha256:"));
  assert.ok(digestOffset > 0);
  complete[digestOffset + 8] = complete[digestOffset + 8] === 0x30 ? 0x31 : 0x30;
  fs.writeFileSync(file, complete);
  assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError && /checksum/i.test(error.message));
});

test("a complete v2 frame with invalid schema is corruption", () => {
  const root = tempRoot();
  const file = logFile(root, "op-bad-schema");
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, Buffer.concat([Buffer.from("O05v2\n"), encodeCommitFrame({ v: 1, kind: "commit" } as never)]));
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /timestamp|snapshot/i.test(error.message));
});

test("operation storage rejects links and non-regular files and repairs restrictive modes", () => {
  const root = tempRoot();
  const operations = path.join(root, "operations");
  fs.mkdirSync(operations, { mode: 0o700 });
  const target = path.join(root, "outside");
  fs.writeFileSync(target, "outside");
  fs.symlinkSync(target, logFile(root, "op-link"));
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /symbolic link|regular file/i.test(error.message));

  fs.unlinkSync(logFile(root, "op-link"));
  fs.mkdirSync(logFile(root, "op-directory"));
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /regular file/i.test(error.message));

  fs.rmSync(logFile(root, "op-directory"), { recursive: true });
  const created = openStore({ root, generateOperationId: () => "op-mode" });
  const id = created.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  assert.equal(fs.statSync(operations).mode & 0o777, 0o700);
  assert.equal(fs.statSync(logFile(root, id)).mode & 0o777, 0o600);
  fs.chmodSync(logFile(root, id), 0o644);
  reopened(root, created.clock);
  assert.equal(fs.statSync(logFile(root, id)).mode & 0o777, 0o600);
});

test("unsupported directory fsync prevents acknowledgment", () => {
  const root = tempRoot();
  const targets = new Map<number, string>();
  const fsLike: StoreFs = {
    ...nodeStoreFs,
    openSync: (target, flags) => {
      const fd = nodeStoreFs.openSync(target, flags);
      targets.set(fd, target);
      return fd;
    },
    fsyncSync: (fd) => {
      if (targets.get(fd) === path.join(root, "operations")) throw Object.assign(new Error("unsupported"), { code: "EINVAL" });
      nodeStoreFs.fsyncSync(fd);
    },
    closeSync: (fd) => {
      nodeStoreFs.closeSync(fd);
      targets.delete(fd);
    },
  };
  assert.throws(() => openStore({ root, fs: fsLike }).store.accept({ request: instruction(), context: context() }), /unsupported/);
});

test("legacy v1 logs replay strictly and rewrite to v2 on first mutation", () => {
  const { store, root, clock } = openStore({ generateOperationId: () => "op-legacy" });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  fs.writeFileSync(logFile(root, operationId), legacyRecords(root, operationId));
  const legacy = reopened(root, clock);
  assert.equal(legacy.load(operationId).state, "accepted");
  legacy.beginResolution(operationId);
  assert.equal(fs.readFileSync(logFile(root, operationId)).subarray(0, 6).toString(), "O05v2\n");
  assert.equal(reopened(root, clock).load(operationId).state, "resolving");
});

test("legacy migration recovers exact artifacts at every rename durability boundary", () => {
  for (const point of ["pre-temp-fsync", "post-temp-fsync", "post-rename"] as const) {
    const first = openStore({ generateOperationId: () => `op-legacy-${point}` });
    const operationId = first.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    const file = logFile(first.root, operationId);
    const legacy = legacyRecords(first.root, operationId);
    fs.writeFileSync(file, legacy);
    const temporary = `${file}.tmp`;
    const acceptedFrame = Buffer.concat([Buffer.from("O05v2\n"), encodeCommitFrame(readStoredRecords(file)[0])]);
    if (point !== "post-rename") {
      fs.writeFileSync(temporary, point === "pre-temp-fsync" ? acceptedFrame.subarray(0, Math.floor(acceptedFrame.length / 2)) : acceptedFrame, { mode: 0o600 });
    } else {
      fs.writeFileSync(file, acceptedFrame, { mode: 0o600 });
    }

    const recovered = reopened(first.root, first.clock);
    assert.equal(recovered.load(operationId).state, "accepted");
    assert.equal(fs.existsSync(temporary), false);
    recovered.beginResolution(operationId);
    assert.equal(reopened(first.root, first.clock).load(operationId).state, "resolving");
  }
});

test("acceptance is durable before the operation id is returned", () => {
  const log: string[] = [];
  const { store, root } = openStore({ fs: instrumentedFs(log) });
  const accepted = store.accept({ request: instruction(), context: context() });
  assert.equal(accepted.replayed, false);
  assert.equal(accepted.snapshot.state, "accepted");
  assert.equal(accepted.snapshot.revision, 1);
  assert.equal(accepted.snapshot.lastSequence, 1);
  assert.equal(accepted.snapshot.scope.workspaceId, "ws-api");
  assert.equal(validateOperationSnapshot(accepted.snapshot).ok, true);

  const file = logFile(root, accepted.snapshot.operationId);
  assert.equal(readStoredRecords(file).length, 1);

  // The operations directory is fsynced after it is created, and the new file is
  // fsynced before its own directory entry.
  const mkdir = log.indexOf("mkdir:dir");
  const firstDirFsync = log.indexOf("fsync:dir");
  const fileFsync = log.lastIndexOf("fsync:file");
  const lastDirFsync = log.lastIndexOf("fsync:dir");
  assert.ok(mkdir >= 0 && mkdir < firstDirFsync, `mkdir before directory fsync: ${log.join(",")}`);
  assert.ok(log.includes("write:file"), `the record is written: ${log.join(",")}`);
  assert.ok(fileFsync >= 0 && fileFsync < lastDirFsync, `file fsync before directory fsync: ${log.join(",")}`);

  // A second process sees the accepted operation, so the acknowledgment was durable.
  const other = reopened(root, clockFrom());
  assert.deepEqual(other.load(accepted.snapshot.operationId), accepted.snapshot);
});

test("a crash during acceptance leaves no operation, and the retry is not duplicated", () => {
  const crash = crashingFs("partial-write");
  const first = openStore({ fs: crash.fs, generateOperationId: () => "op-crash-1" });
  assert.throws(() => first.store.accept({ request: instruction(), context: context() }), /simulated crash during append/);
  assert.equal(crash.crashed(), true);
  // The failed append poisons this instance rather than letting it write a second copy.
  assert.throws(
    () => first.store.accept({ request: instruction(), context: context() }),
    (error) => isStoreError(error, "execution_failure"),
  );

  const restarted = reopened(first.root, first.clock, { generateOperationId: () => "op-crash-1" });
  assert.deepEqual(restarted.operationIds(), []);
  assert.equal(restarted.tornTails.length, 1);
  const accepted = restarted.accept({ request: instruction(), context: context() });
  assert.equal(accepted.snapshot.operationId, "op-crash-1");
  assert.equal(restarted.repairs.length, 1);
  assert.equal(restarted.repairs[0].action, "truncated");
  assert.deepEqual(restarted.operationIds(), ["op-crash-1"]);
  assert.equal(readStoredRecords(logFile(first.root, "op-crash-1")).length, 1);
});

test("a record written before the crash is an accepted operation, not a duplicate", () => {
  const crash = crashingFs("fsync-after-write");
  const first = openStore({ fs: crash.fs, generateOperationId: () => "op-crash-2" });
  assert.throws(() => first.store.accept({ request: instruction(), context: context() }), /simulated crash before fsync/);
  const restarted = reopened(first.root, first.clock);
  assert.deepEqual(restarted.operationIds(), ["op-crash-2"]);
  // The same tool call replays the unacknowledged operation instead of starting a second.
  const replayed = restarted.accept({ request: instruction(), context: context() });
  assert.equal(replayed.replayed, true);
  assert.equal(replayed.snapshot.operationId, "op-crash-2");
  assert.equal(restarted.operationIds().length, 1);
});

test("the same tool call replays its operation and a changed request is rejected", () => {
  const { store, root } = openStore();
  const accepted = store.accept({ request: { instruction: INSTRUCTION, constraints: ["stay inside src"] }, context: context() });
  const replayed = store.accept({
    request: { constraints: ["stay inside src"], instruction: INSTRUCTION },
    context: context(),
  });
  assert.equal(replayed.replayed, true);
  assert.equal(replayed.snapshot.operationId, accepted.snapshot.operationId);
  assert.deepEqual(store.operationIds(), [accepted.snapshot.operationId]);

  assert.throws(
    () => store.accept({ request: instruction("Change the timeout to 60000."), context: context() }),
    (error) => error instanceof DuplicateRequestConflictError && error.code === "revision_conflict",
  );
  assert.equal(store.load(accepted.snapshot.operationId).revision, accepted.snapshot.revision);
  assert.equal(readStoredRecords(logFile(root, accepted.snapshot.operationId)).length, 1);

  // A different tool call is a different unit of work, not a duplicate.
  const other = store.accept({ request: instruction(), context: context({ toolCallId: "call-2" }) });
  assert.notEqual(other.snapshot.operationId, accepted.snapshot.operationId);
  assert.equal(store.operationIds().length, 2);
  assert.notEqual(deriveDeduplicationKey(context()), deriveDeduplicationKey(context({ toolCallId: "call-2" })));
});

test("identity comes from the trusted context, never from the request", () => {
  const { store } = openStore();
  assert.throws(
    () => store.accept({ request: { instruction: INSTRUCTION, actorId: "user-2" } as OperateRequest, context: context() }),
    (error) => error instanceof ContractViolation,
  );
  assert.throws(
    () => store.accept({ request: instruction(), context: { ...context(), turnRevision: 0 } }),
    (error) => error instanceof ContractViolation,
  );
  assert.throws(
    () => store.accept({ request: { action: "inspect", operationId: "op-anything" } as OperateRequest, context: context() }),
    (error) => isStoreError(error, "unsupported_request"),
  );
  assert.equal(store.operationIds().length, 0);
});

test("only internally generated ids name files", () => {
  const { store, root } = openStore({ generateOperationId: () => "../escape" });
  assert.throws(
    () => store.accept({ request: instruction(), context: context() }),
    (error) => error instanceof InvalidOperationIdError,
  );
  assert.deepEqual(fs.readdirSync(root), []);
  assert.throws(() => store.load("../../etc/passwd"), (error) => error instanceof InvalidOperationIdError);
  assert.throws(() => store.inspect("op-ok/../escape"), (error) => error instanceof InvalidOperationIdError);
});

test("budgets narrow the policy ceiling and persist across restarts", () => {
  const { store, root, clock } = openStore();
  const accepted = store.accept({
    request: instruction(),
    context: context(),
    budgets: { maxSteps: 1, maxSelectionRounds: 1, operationDeadlineMs: 1_000 },
  });
  const operationId = accepted.snapshot.operationId;
  assert.equal(accepted.snapshot.budgets.maxSteps, 1);
  assert.equal(accepted.snapshot.deadlineAt, accepted.snapshot.createdAt + 1_000);
  assert.throws(
    () => store.accept({ request: instruction(), context: context({ toolCallId: "call-2" }), budgets: { maxSteps: 99 } }),
    (error) => error instanceof ContractViolation,
  );

  queueEdit(store, operationId);
  assert.throws(() => queueEdit(store, operationId, "second"), (error) => isStoreError(error, "budget_exceeded"));
  store.consumeBudget(operationId, { selectionRounds: 1 });
  assert.throws(() => store.consumeBudget(operationId, { selectionRounds: 1 }), (error) => isStoreError(error, "budget_exceeded"));
  assert.throws(() => store.consumeBudget(operationId, { generationCalls: 0 }), (error) => error instanceof ContractViolation);

  const restarted = reopened(root, clock);
  const persisted = restarted.load(operationId);
  assert.equal(persisted.budgets.maxSteps, 1);
  assert.equal(persisted.usage.selectionRounds, 1);
  assert.equal(persisted.steps.length, 1);
  assert.equal(persisted.steps[0].state, "queued");

  clock.advance(2_000);
  const expired = restarted.expire(operationId);
  assert.equal(expired.state, "expired");
  assert.equal(expired.steps[0].state, "cancelled");
  assert.equal(expired.pendingDecisions.length, 0);
  assert.equal(validateOperationSnapshot(expired).ok, true);
  assert.equal(restarted.expire(operationId).lastSequence, expired.lastSequence);
});

test("step intent is recorded before dispatch and outcomes need evidence", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.beginResolution(operationId);
  const queued = queueEdit(store, operationId);
  assert.equal(queued.state, "resolving");
  assert.equal(queued.steps[0].state, "queued");
  assert.equal(queued.usage.steps, 1);

  assert.throws(() => store.startStep(operationId, "step-1", { argDigest: "not-a-digest" }), (error) => error instanceof ContractViolation);
  const running = store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A, backendOperationId: "backend-7" });
  assert.equal(running.state, "running");
  assert.equal(running.steps[0].state, "running");
  assert.equal(running.steps[0].attempts, 1);
  assert.equal(running.steps[0].idempotencyKey, `${operationId}/step-1/1`);
  assert.equal(running.steps[0].backendOperationId, "backend-7");
  assert.equal(running.usage.attempts, 1);

  const dispatched = store.events(operationId).filter((event) => event.phase === "dispatched");
  assert.equal(dispatched.length, 1);
  assert.equal(dispatched[0].argDigest, PAYLOAD_A);
  assert.equal(store.events(operationId).some((event) => event.phase === "succeeded"), false);

  assert.throws(
    () => store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded" }),
    (error) => isStoreError(error, "invalid_transition"),
  );
  const done = store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  assert.equal(done.state, "completed");
  assert.equal(done.steps[0].state, "succeeded");
  assert.equal(done.steps[0].evidenceRefs?.[0], "change-1");
  assert.equal(done.result?.state, "completed");
  assert.equal(done.result?.changed, true);
  assert.equal(validateCompactOperationResult(store.project(operationId)).ok, true);
  const sequences = store.events(operationId).map((event) => event.sequence);
  assert.deepEqual(sequences, sequences.map((_, index) => index + 1));
});

test("startStep cannot bypass current user approval for a queued mutation", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);

  assert.throws(
    () => store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A }),
    (error) => isStoreError(error, "permission_denied"),
  );
  const unchanged = store.load(operationId);
  assert.equal(unchanged.steps[0].state, "queued");
  assert.equal(unchanged.usage.attempts, 0);
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);
});

test("a recorded decision is bound, consumed once, and never replayed", () => {
  const authority = new DispatchAuthority();
  const { store, clock } = openStore({ dispatchAuthority: authority });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts: timeout 9000 -> 30000?",
    payloadDigest: PAYLOAD_A,
  });
  assert.equal(waiting.state, "awaiting_approval");
  assert.equal(waiting.steps[0].state, "awaiting_approval");
  assert.equal(waiting.pendingDecisions[0].revision, waiting.revision);
  assert.equal(store.project(operationId).decision?.decisionId, "dec-1");

  const record = recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A });
  const recorded = store.recordDecision(record, context());
  assert.equal(recorded.revision, waiting.revision, "recording a decision does not revise the operation");
  assert.equal(store.recordedDecisions(operationId).length, 1);
  assert.throws(() => store.recordDecision(record, context()), (error) => isStoreError(error, "decision_replayed"));

  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision + 4, "rec-1"), context: context() }),
    (error) => isStoreError(error, "revision_conflict"),
  );
  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-missing"), context: context() }),
    (error) => isStoreError(error, "decision_mismatch"),
  );
  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"), context: context({ actorId: "user-2" }) }),
    (error) => isStoreError(error, "permission_denied"),
  );

  const outcome = store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"), context: context() });
  assert.equal(outcome.outcome, "dispatch");
  assert.equal(outcome.snapshot.steps[0].state, "running");
  assert.equal(outcome.snapshot.steps[0].idempotencyKey, `${operationId}/step-1/1`);
  assert.equal(outcome.snapshot.pendingDecisions.length, 0);
  assert.equal(outcome.snapshot.usage.attempts, 1);
  assert.equal(store.events(operationId).filter((event) => event.phase === "dispatched").length, 1);
  assert.equal(store.recordedDecisions(operationId)[0].usedAt, clock.now());
  if (outcome.outcome !== "dispatch") assert.fail("expected dispatch authorization");
  const claims = { operationId, stepId: "step-1", capability: "file.edit", version: "1.0.0", payloadDigest: PAYLOAD_A, attempt: 1 };
  assert.equal(authority.consume(outcome.dispatchToken, claims), true);
  assert.equal(authority.consume(outcome.dispatchToken, claims), false);

  // Replaying the same resolution after it was consumed is refused.
  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"), context: context() }),
    (error) => isStoreError(error, "decision_mismatch"),
  );
  assert.equal(store.events(operationId).filter((event) => event.phase === "dispatched").length, 1);
});

test("recording a delayed decision preserves same-revision lifecycle timestamps", () => {
  const { store, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-delayed",
    decisionClass: "user_authorization",
    question: "Apply the delayed edit?",
    payloadDigest: PAYLOAD_A,
  });
  clock.advance(1_000);
  const recorded = store.recordDecision(recordedDecision(waiting, { decisionId: "dec-delayed", payloadDigest: PAYLOAD_A }), context());
  assert.equal(recorded.revision, waiting.revision);
  assert.equal(recorded.updatedAt, waiting.updatedAt);
  const decisionEvent = store.events(operationId).findLast((event) => event.phase === "decision_recorded");
  assert.equal(decisionEvent?.at, clock.now());
  const resumed = store.resume({ request: resumeRequest(operationId, "dec-delayed", waiting.revision, "rec-1"), context: context() });
  assert.equal(resumed.outcome, "dispatch");
  assert.equal(resumed.snapshot.revision, waiting.revision + 1);
  assert.equal(resumed.snapshot.updatedAt, clock.now());
});

test("a resume from a superseded user turn is refused, and the watermark is durable", () => {
  const { store, root, clock } = openStore();
  const accepted = store.accept({
    request: instruction(),
    context: context({ turnId: "turn-2", turnRevision: 2 }),
  });
  const operationId = accepted.snapshot.operationId;
  queueEdit(store, operationId);

  // An answer supplied from a later trusted turn advances the watermark.
  const asked = store.requireDecision(operationId, "step-1", {
    decisionId: "ask-1",
    decisionClass: "additional_input",
    question: "Which file should change: src/config.ts or src/configuration.ts?",
    payloadDigest: canonicalDigest({ kind: "additional_input", inputs: { target: "src/config.ts" } }),
  });
  const supplied = store.resume({
    request: {
      action: "resume",
      operationId,
      decisionId: "ask-1",
      expectedRevision: asked.revision,
      resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } },
    },
    context: context({ turnId: "turn-4", turnRevision: 4 }),
  });
  assert.equal(supplied.outcome, "supply_input");

  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A }), context());

  // A replayed tool call from the older turn is refused and consumes nothing.
  assert.throws(
    () =>
      store.resume({
        request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"),
        context: context({ turnId: "turn-3", turnRevision: 3 }),
      }),
    (error) => isStoreError(error, "revision_conflict"),
  );
  assert.equal(store.pendingDecisions(operationId).length, 1);
  assert.equal(store.load(operationId).steps[0].state, "awaiting_approval");
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);

  // The refusal is a durable property of the operation, not of this process.
  const restarted = reopened(root, clock);
  assert.throws(
    () =>
      restarted.resume({
        request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"),
        context: context({ turnId: "turn-3", turnRevision: 3 }),
      }),
    (error) => isStoreError(error, "revision_conflict"),
  );

  // The current trusted turn resumes it.
  const outcome = restarted.resume({
    request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"),
    context: context({ turnId: "turn-4", turnRevision: 4 }),
  });
  assert.equal(outcome.outcome, "dispatch");
  assert.equal(outcome.snapshot.steps[0].state, "running");
});

test("a changed payload invalidates the recorded approval", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A }), context());

  // The file changed while the decision was pending, so the question is re-proposed.
  const reproposed = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_B,
  });
  assert.equal(reproposed.state, "awaiting_approval");
  assert.equal(store.stepPayloadDigest(operationId, "step-1"), PAYLOAD_B);

  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", reproposed.revision, "rec-1"), context: context() }),
    (error) => isStoreError(error, "validation_failure"),
  );
  assert.equal(store.load(operationId).steps[0].state, "awaiting_approval");
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);
});

test("a recorded denial skips the step and never dispatches it", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A, outcome: "denied" }), context());
  const outcome = store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"), context: context() });
  assert.equal(outcome.outcome, "refuse_step");
  assert.equal(outcome.snapshot.steps[0].state, "skipped");
  assert.equal(outcome.snapshot.state, "failed");
  assert.equal(outcome.snapshot.pendingDecisions.length, 0);
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);
  assert.equal(store.events(operationId).some((event) => event.decisionCode === "denied"), true);
});

test("a trusted policy approval cannot satisfy a user-only requirement", () => {
  const flow = (policy: ApprovalRequirement) => {
    const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: policy }) });
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(store, operationId);
    const waiting = store.requireDecision(operationId, "step-1", {
      decisionId: "dec-1",
      decisionClass: "user_authorization",
      question: "Apply the change to src/config.ts?",
      payloadDigest: PAYLOAD_A,
    });
    store.recordDecision(
      recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A, policySource: "trusted_policy" }),
      context(),
    );
    return { store, operationId, waiting };
  };

  const userOnly = flow("user");
  assert.throws(
    () => userOnly.store.resume({ request: resumeRequest(userOnly.operationId, "dec-1", userOnly.waiting.revision, "rec-1"), context: context() }),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(userOnly.store.load(userOnly.operationId).steps[0].state, "awaiting_approval");
  assert.equal(userOnly.store.pendingDecisions(userOnly.operationId).length, 1);
  assert.equal(userOnly.store.events(userOnly.operationId).some((event) => event.phase === "dispatched"), false);

  const automatic = flow("policy");
  const outcome = automatic.store.resume({
    request: resumeRequest(automatic.operationId, "dec-1", automatic.waiting.revision, "rec-1"),
    context: context(),
  });
  assert.equal(outcome.outcome, "dispatch");
  assert.equal(outcome.snapshot.steps[0].state, "running");
});

test("a user approval cannot satisfy a policy-only requirement", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "policy" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-policy-only",
    decisionClass: "user_authorization",
    question: "Does trusted policy authorize this change?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-policy-only", payloadDigest: PAYLOAD_A }), context());

  assert.throws(
    () =>
      store.resume({
        request: resumeRequest(operationId, "dec-policy-only", waiting.revision, "rec-1"),
        context: context(),
      }),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(store.load(operationId).steps[0].state, "awaiting_approval");
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);
});

test("additional input cannot answer a user decision, and forged approvals are rejected", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
  });
  assert.throws(
    () =>
      store.resume({
        request: {
          action: "resume",
          operationId,
          decisionId: "dec-1",
          expectedRevision: waiting.revision,
          resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } },
        },
        context: context(),
      }),
    (error) => isStoreError(error, "decision_mismatch"),
  );
  assert.equal(store.load(operationId).steps[0].state, "awaiting_approval");
  assert.equal(store.pendingDecisions(operationId).length, 1);
  assert.throws(
    () =>
      store.resume({
        request: {
          action: "resume",
          operationId,
          decisionId: "dec-1",
          expectedRevision: waiting.revision,
          resolution: { kind: "additional_input", inputs: { yes: true } },
        },
        context: context(),
      }),
    (error) => error instanceof ContractViolation,
  );

  // A genuine additional-input question resumes its own operation with typed provenance.
  const asking = store.accept({ request: instruction(), context: context({ toolCallId: "call-2" }) }).snapshot.operationId;
  queueEdit(store, asking);
  const asked = store.requireDecision(asking, "step-1", {
    decisionId: "ask-1",
    decisionClass: "additional_input",
    question: "Which file should change: src/config.ts or src/configuration.ts?",
    payloadDigest: canonicalDigest({ kind: "additional_input", inputs: { target: "src/config.ts" } }),
  });
  assert.equal(asked.state, "needs_input");
  const supplied = store.resume({
    request: {
      action: "resume",
      operationId: asking,
      decisionId: "ask-1",
      expectedRevision: asked.revision,
      resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } },
    },
    context: context(),
  });
  assert.equal(supplied.outcome, "supply_input");
  assert.deepEqual(supplied.resolution.inputs, { target: "src/config.ts" });
  assert.deepEqual(store.resolutionsFor(asking), [
    { decisionId: "ask-1", resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } } },
  ]);
  assert.equal(store.load(asking).pendingDecisions.length, 0);
  assert.equal(store.load(asking).state, "running");
  assert.equal(store.load(operationId).steps[0].state, "awaiting_approval");
});

test("cancellation after a committed effect reports partial and keeps the evidence", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.queueStep(operationId, { key: "read_config", capability: "file.read" });
  queueEdit(store, operationId, "edit_config");
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["read-1"] });
  assert.equal(store.load(operationId).state, "running");

  const cancelled = store.requestCancel(operationId, { reason: "the person changed their mind" });
  assert.equal(cancelled.steps[0].state, "succeeded");
  assert.deepEqual(cancelled.steps[0].evidenceRefs, ["read-1"]);
  assert.equal(cancelled.steps[1].state, "cancelled");
  assert.equal(cancelled.state, "partial");
  assert.equal(validateOperationSnapshot(cancelled).ok, true);
  assert.equal(cancelled.result?.state, "partial");

  const repeat = store.requestCancel(operationId);
  assert.equal(repeat.lastSequence, cancelled.lastSequence);

  const restarted = reopened(root, clock);
  assert.equal(restarted.load(operationId).state, "partial");

  // An operation cancelled before it queued any work settles as cancelled.
  const empty = store.accept({ request: instruction(), context: context({ toolCallId: "call-3" }) }).snapshot.operationId;
  assert.equal(store.requestCancel(empty).state, "cancelled");
});

test("cancellation records durable abort intent for every running step", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });

  assert.equal(store.requestCancel(operationId).state, "cancel_requested");
  assert.deepEqual(store.pendingAbortStepIds(operationId), ["step-1"]);
  assert.deepEqual(reopened(root, clock).pendingAbortStepIds(operationId), ["step-1"]);
});

test("partial settlement summary counts skipped work as not applied", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.queueStep(operationId, { key: "read", capability: "file.read" });
  queueEdit(store, operationId, "edit");
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["read-1"] });
  const settled = store.requestCancel(operationId);

  assert.equal(settled.state, "partial");
  assert.equal(settled.result?.summary, "1 of 2 steps completed; 1 did not apply and no rollback was attempted.");
});

test("a caller cannot escalate a trusted none retry policy to idempotent", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry: { class: "none", maxAttempts: 1 } }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId, "edit_config");
  store.queueStep(operationId, { key: "read_config", capability: "file.read" });
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const failed = store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });
  assert.equal(failed.state, "running", "the queued second step keeps the operation open");
  assert.equal(failed.steps[0].state, "failed");

  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "idempotent", maxAttempts: 2 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "none", maxAttempts: 1 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.equal(store.load(operationId).steps[0].state, "failed", "a refused retry records nothing");
  assert.equal(store.load(operationId).usage.attempts, 1);
});

test("a non-retryable failure settles the operation instead of leaving it running", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry: { class: "none", maxAttempts: 1 } }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });

  const failed = store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: false },
  });

  assert.equal(failed.state, "failed");
  assert.equal(failed.result?.state, "failed");
});

test("an attempt-exhausted retryable failure settles the operation", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 1 };
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });

  const failed = store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the final attempt failed", retryable: true },
  });

  assert.equal(failed.state, "failed");
  assert.equal(failed.result?.state, "failed");
});

test("a caller cannot raise the trusted retry attempt ceiling", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry: { class: "idempotent", maxAttempts: 1 } }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId, "edit_config");
  store.queueStep(operationId, { key: "read_config", capability: "file.read" });
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });

  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "idempotent", maxAttempts: 2 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "idempotent", maxAttempts: 1 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.equal(store.load(operationId).steps[0].state, "failed");
  assert.equal(store.load(operationId).usage.attempts, 1);
});

test("a caller cannot substitute a conflicting retry class", () => {
  const { store } = openStore({
    capabilityMetadata: (capability) => ({
      ...(testMetadata(capability)!),
      approval: "none",
      retry: { class: "reconcile_required", maxAttempts: 3 },
    }),
  });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId, "edit_config");
  store.queueStep(operationId, { key: "read_config", capability: "file.read" });
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });

  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "idempotent", maxAttempts: 3 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry: { class: "reconcile_required", maxAttempts: 3 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.equal(store.load(operationId).steps[0].state, "failed");
  assert.equal(store.events(operationId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
});

test("a failed step restarts within its trusted idempotent retry ceiling", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 3 };
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId, "edit_config");
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const failed = store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });
  assert.equal(failed.state, "running");

  const retried = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry }, context());
  assert.equal(retried.snapshot.steps[0].state, "running");
  assert.equal(retried.snapshot.steps[0].attempts, 2);
  assert.equal(retried.snapshot.usage.attempts, 2);
  assert.equal(store.events(operationId).filter((event) => event.decisionCode === "retry_allowed").length, 1);
});

test("retry authorization is fresh and invalidates the previous attempt", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  const authority = new DispatchAuthority();
  const { store } = openStore({ dispatchAuthority: authority, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "user", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", { decisionId: "dec-attempt", decisionClass: "user_authorization", question: "Apply?", payloadDigest: PAYLOAD_A });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-attempt", payloadDigest: PAYLOAD_A }), context());
  const first = store.resume({ request: resumeRequest(operationId, "dec-attempt", waiting.revision, "rec-1"), context: context() });
  if (first.outcome !== "dispatch") assert.fail("expected first dispatch");
  store.recordStepOutcome(operationId, "step-1", { outcome: "failed", error: { code: "execution_failure", message: "retry", retryable: true } });
  const second = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context());
  const firstClaims = { operationId, stepId: "step-1", capability: "file.edit", version: "1.0.0", payloadDigest: PAYLOAD_A, attempt: 1 };
  const secondClaims = { ...firstClaims, attempt: 2 };
  assert.equal(authority.consume(first.dispatchToken, firstClaims), false);
  assert.equal(authority.consume(second.dispatchToken, { ...secondClaims, capability: "file.write" }), false);
  assert.equal(authority.consume(second.dispatchToken, secondClaims), false);

});

test("a fresh retry authorization succeeds exactly once", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  const authority = new DispatchAuthority();
  const { store } = openStore({ dispatchAuthority: authority, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "user", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", { decisionId: "dec-retry", decisionClass: "user_authorization", question: "Apply?", payloadDigest: PAYLOAD_A });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-retry", payloadDigest: PAYLOAD_A }), context());
  const first = store.resume({ request: resumeRequest(operationId, "dec-retry", waiting.revision, "rec-1"), context: context() });
  if (first.outcome !== "dispatch") assert.fail("expected first dispatch");
  store.recordStepOutcome(operationId, "step-1", { outcome: "failed", error: { code: "execution_failure", message: "retry", retryable: true } });

  const fresh = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context());
  const claims = { operationId, stepId: "step-1", capability: "file.edit", version: "1.0.0", payloadDigest: PAYLOAD_A, attempt: 2 };
  assert.equal(authority.consume(fresh.dispatchToken, claims), true);
  assert.equal(authority.consume(fresh.dispatchToken, claims), false);
});

test("retry starts without stale attempt-specific backend fields", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A, backendOperationId: "backend-old" });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });

  const retried = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry }, context());
  assert.equal(retried.snapshot.steps[0].backendOperationId, undefined);
  assert.equal(retried.snapshot.steps[0].error, undefined);
  assert.equal(retried.snapshot.steps[0].endedAt, undefined);
});

test("policy-required mutations dispatch and retry only with trusted policy evidence", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  let approval: ApprovalRequirement = "policy";
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: approval, retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);

  assert.throws(
    () => store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A }),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(store.events(operationId).some((event) => event.phase === "dispatched"), false);

  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-policy",
    decisionClass: "user_authorization",
    question: "Does trusted policy authorize this change?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(
    recordedDecision(waiting, {
      decisionId: "dec-policy",
      payloadDigest: PAYLOAD_A,
      policySource: "trusted_policy",
    }),
    context(),
  );
  const dispatched = store.resume({
    request: resumeRequest(operationId, "dec-policy", waiting.revision, "rec-1"),
    context: context(),
  });
  assert.equal(dispatched.outcome, "dispatch");
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });
  const retried = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context());
  assert.equal(retried.snapshot.steps[0].state, "running");

  approval = "none";
  const unapprovedId = store.accept({ request: instruction(), context: context({ toolCallId: "call-policy-retry" }) }).snapshot.operationId;
  queueEdit(store, unapprovedId);
  store.startStep(unapprovedId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(unapprovedId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });
  approval = "policy";
  assert.throws(
    () => store.retryStep(unapprovedId, "step-1", { argDigest: PAYLOAD_A, retry }, context()),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(store.events(unapprovedId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
});

test("retry requires a retryable failure before the operation deadline", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  const clock = clockFrom();
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry }), clock });
  const expiredId = store.accept({
    request: instruction(),
    context: context(),
    budgets: { operationDeadlineMs: 1_000 },
  }).snapshot.operationId;
  queueEdit(store, expiredId);
  store.startStep(expiredId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(expiredId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "try again", retryable: true },
  });
  clock.advance(1_000);
  assert.throws(
    () => store.retryStep(expiredId, "step-1", { argDigest: PAYLOAD_A, retry }, context()),
    (error) => isStoreError(error, "deadline_exceeded"),
  );
  assert.equal(store.events(expiredId).filter((event) => event.decisionCode === "retry_allowed").length, 0);

  const terminalFailureId = store.accept({
    request: instruction(),
    context: context({ toolCallId: "call-not-retryable" }),
  }).snapshot.operationId;
  queueEdit(store, terminalFailureId);
  store.startStep(terminalFailureId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(terminalFailureId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "do not retry", retryable: false },
  });
  assert.throws(
    () => store.retryStep(terminalFailureId, "step-1", { argDigest: PAYLOAD_A, retry }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.equal(store.events(terminalFailureId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
});

test("expired consumed approvals cannot authorize retries", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  for (const [requirement, policySource] of [
    ["user", "user_ui"],
    ["policy", "trusted_policy"],
  ] as const) {
    const clock = clockFrom();
    const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: requirement, retry }), clock });
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(store, operationId);
    const waiting = store.requireDecision(operationId, "step-1", {
      decisionId: `dec-${requirement}`,
      decisionClass: "user_authorization",
      question: "Authorize the exact change?",
      payloadDigest: PAYLOAD_A,
      ttlMs: 1_000,
    });
    store.recordDecision(
      recordedDecision(waiting, {
        decisionId: `dec-${requirement}`,
        payloadDigest: PAYLOAD_A,
        policySource,
        expiresAt: waiting.pendingDecisions[0].expiresAt,
      }),
      context(),
    );
    store.resume({
      request: resumeRequest(operationId, `dec-${requirement}`, waiting.revision, "rec-1"),
      context: context(),
    });
    store.recordStepOutcome(operationId, "step-1", {
      outcome: "failed",
      error: { code: "execution_failure", message: "the write did not complete", retryable: true },
    });
    clock.advance(1_000);

    assert.throws(
      () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context()),
      (error) => isStoreError(error, "decision_expired"),
    );
    assert.equal(store.events(operationId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
  }
});

test("a user-approved failed write cannot retry with a changed digest", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "user", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-retry",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-retry", payloadDigest: PAYLOAD_A }), context());
  const dispatched = store.resume({
    request: resumeRequest(operationId, "dec-retry", waiting.revision, "rec-1"),
    context: context(),
  });
  assert.equal(dispatched.outcome, "dispatch");
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });

  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_B, retry }, context()),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(store.load(operationId).steps[0].state, "failed");
  assert.equal(store.load(operationId).usage.attempts, 1);
  assert.equal(store.events(operationId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
});

test("a failed write cannot retry after policy requires an approval it never received", () => {
  const retry: RetryStepInput["retry"] = { class: "idempotent", maxAttempts: 2 };
  let approval: ApprovalRequirement = "none";
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: approval, retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "the write did not complete", retryable: true },
  });
  approval = "user";

  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context()),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.equal(store.load(operationId).steps[0].state, "failed");
  assert.equal(store.events(operationId).filter((event) => event.decisionCode === "retry_allowed").length, 0);
});

test("an unknown outcome is durable, cannot expire, and leaves only by reconciliation", () => {
  const { store, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A, idempotencyKey: `${operationId}/step-1` });
  const unknown = store.markOutcomeUnknown(operationId, "step-1");
  assert.equal(unknown.state, "reconciling");
  assert.equal(unknown.steps[0].state, "outcome_unknown");
  assert.equal(unknown.unknownOutcomes[0].dispatchDigest, PAYLOAD_A);

  assert.throws(
    () => store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["guess"] }),
    (error) => isStoreError(error, "invalid_transition"),
  );
  clock.advance(60 * 60 * 1000);
  assert.equal(store.expire(operationId).state, "reconciling", "unknown effects never expire away");

  const reconciled = store.reconcileStep(operationId, "step-1", { conclusion: "succeeded", evidenceRefs: ["backend-1"] });
  assert.equal(reconciled.state, "completed");
  assert.equal(reconciled.steps[0].state, "succeeded");
  assert.equal(reconciled.unknownOutcomes.length, 0);
  assert.equal(store.events(operationId).some((event) => event.phase === "reconciled"), true);
});

test("a reconciled failure leaves reconciling and settles the operation", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.markOutcomeUnknown(operationId, "step-1");

  const reconciled = store.reconcileStep(operationId, "step-1", {
    conclusion: "failed",
    error: { code: "execution_failure", message: "the backend rejected the write", retryable: false },
  });

  assert.equal(reconciled.state, "failed");
  assert.equal(reconciled.unknownOutcomes.length, 0);
  assert.deepEqual(reconciled.result?.state, "failed");
});

test("a decision that expires releases the step without inventing an answer", () => {
  const { store, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-1",
    decisionClass: "user_authorization",
    question: "Apply the change to src/config.ts?",
    payloadDigest: PAYLOAD_A,
    ttlMs: 1_000,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-1", payloadDigest: PAYLOAD_A, expiresAt: waiting.pendingDecisions[0].expiresAt }), context());
  clock.advance(5_000);
  assert.throws(
    () => store.resume({ request: resumeRequest(operationId, "dec-1", waiting.revision, "rec-1"), context: context() }),
    (error) => isStoreError(error, "decision_expired"),
  );
  const expired = store.expire(operationId);
  assert.equal(expired.pendingDecisions.length, 0);
  assert.equal(expired.steps[0].state, "cancelled");
  assert.equal(expired.state, "cancelled");
  assert.equal(store.events(operationId).some((event) => event.decisionCode === "decision_expired"), true);
});

test("a torn final write is recovered while interior corruption is refused", () => {
  const torn = openStore();
  const installed = installFixture(torn.root, "torn-tail-write.json");
  const store = reopened(torn.root, torn.clock);
  assert.equal(store.tornTails.length, 1);
  assert.equal(store.load(installed.operationId).state, "accepted");
  assert.equal(store.load(installed.operationId).lastSequence, 1);
  assert.deepEqual(store.events(installed.operationId).map((event) => event.sequence), [1]);

  queueEdit(store, installed.operationId);
  assert.equal(store.repairs.length, 0, "the atomic v1 rewrite supersedes its torn tail without mutating it in place");
  assert.equal(readStoredRecords(installed.file).length, 2);
  const afterRepair = reopened(torn.root, torn.clock);
  assert.deepEqual(afterRepair.tornTails, []);
  assert.equal(afterRepair.load(installed.operationId).steps.length, 1);

  const corrupt = openStore();
  installFixture(corrupt.root, "interior-corruption.json");
  assert.throws(
    () => reopened(corrupt.root, corrupt.clock),
    (error) => error instanceof OperationLogCorruptError && /line 2 is not JSON/.test(error.message),
  );

  // A parseable line that is otherwise a commit but has no snapshot is refused.
  const tampered = openStore();
  const accepted = tampered.store.accept({ request: instruction(), context: context() });
  appendStoredRecord(logFile(tampered.root, accepted.snapshot.operationId), { v: 1, kind: "commit", at: 1, turnRevision: 1 });
  assert.throws(
    () => reopened(tampered.root, tampered.clock),
    (error) => error instanceof OperationLogCorruptError && /snapshot/.test(error.message),
  );

  // The trusted turn revision is required, not defaulted: a commit without one is refused.
  const unversioned = openStore();
  const versioned = unversioned.store.accept({ request: instruction(), context: context() });
  appendStoredRecord(logFile(unversioned.root, versioned.snapshot.operationId), { v: 1, kind: "commit", at: 1 });
  assert.throws(
    () => reopened(unversioned.root, unversioned.clock),
    (error) => error instanceof OperationLogCorruptError && /turn revision/.test(error.message),
  );
});

test("a log whose event sequence does not follow is corrupt, not silently truncated", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.beginResolution(operationId);
  const file = logFile(root, operationId);
  const records = readStoredRecords(file) as Array<{ snapshot: OperationSnapshot; events: Array<{ sequence: number }> }>;
  const forged = JSON.parse(JSON.stringify(records[records.length - 1])) as (typeof records)[number];
  forged.snapshot.lastSequence = 99;
  forged.events[0].sequence = 99;
  appendStoredRecord(file, forged);
  assert.throws(
    () => reopened(root, clock),
    (error) => error instanceof OperationLogCorruptError && /lastSequence/.test(error.message),
  );
});

test("replay rejects lifecycle regression, skipped revisions, and immutable snapshot changes", () => {
  for (const mutate of [
    (record: any) => void (record.snapshot.state = "accepted"),
    (record: any) => void (record.snapshot.revision += 2),
    (record: any) => void (record.snapshot.actor.actorId = "user-2"),
    (record: any) => void (record.snapshot.request.instruction = "tampered"),
    (record: any) => void (record.snapshot.createdAt += 1),
  ]) {
    const { store, root, clock } = openStore();
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    store.beginResolution(operationId);
    const file = logFile(root, operationId);
    const records = readStoredRecords(file);
    const forged = structuredClone(records[records.length - 1]);
    mutate(forged);
    appendStoredRecord(file, forged);
    assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
  }
});

test("replay rejects forged forward transitions, missing trigger facts, and eventless changes", () => {
  for (const mutate of [
    (record: any) => void (record.snapshot.state = "completed"),
    (record: any) => {
      record.snapshot.steps[0].state = "succeeded";
      record.snapshot.steps[0].endedAt = record.at;
    },
    (record: any) => {
      record.events = [];
      record.snapshot.lastSequence -= 1;
    },
  ]) {
    const { store, root, clock } = openStore();
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(store, operationId);
    const file = logFile(root, operationId);
    const records = readStoredRecords(file);
    const forged = structuredClone(records[records.length - 1]);
    forged.snapshot.revision += 1;
    mutate(forged);
    appendStoredRecord(file, forged);
    assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
  }
});

test("replay preserves the exact ordered step set and stable step ids", () => {
  for (const mutate of [
    (record: any) => void record.snapshot.steps.pop(),
    (record: any) => void record.snapshot.steps.reverse(),
    (record: any) => void (record.snapshot.steps[0].stepId = "step-forged"),
  ]) {
    const { store, root, clock } = openStore();
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(store, operationId, "first");
    queueEdit(store, operationId, "second");
    const file = logFile(root, operationId);
    const records = readStoredRecords(file);
    const forged = structuredClone(records[records.length - 1]);
    forged.snapshot.revision += 1;
    mutate(forged);
    appendStoredRecord(file, forged);
    assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
  }
});

test("replay validates pending decisions, unknown outcomes, and recorded resolutions", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.requireDecision(operationId, "step-1", {
    decisionId: "ask-replay",
    decisionClass: "additional_input",
    question: "Which target?",
    payloadDigest: canonicalDigest({ kind: "additional_input", inputs: { target: "src/config.ts" } }),
  });
  const file = logFile(root, operationId);
  const records = readStoredRecords(file);
  const forged = structuredClone(records[records.length - 1]);
  forged.snapshot.revision += 1;
  forged.snapshot.pendingDecisions = [];
  forged.resolutions = [{ decisionId: "unknown", resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } } }];
  appendStoredRecord(file, forged);
  assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
});

test("replay rejects same-revision facts and event phases that imply the wrong trigger", () => {
  for (const mutate of [
    (record: any) => void (record.snapshot.usage.selectionRounds += 1),
    (record: any) => void (record.events[0].phase = "progress"),
    (record: any) => void (record.events[0].decisionCode = "approval_denied"),
  ]) {
    const { store, root, clock } = openStore();
    const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(store, operationId);
    const file = logFile(root, operationId);
    const records = readStoredRecords(file);
    const forged = structuredClone(records[records.length - 1]);
    mutate(forged);
    appendStoredRecord(file, forged);
    assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
  }
});

test("replay rejects delayed same-revision commits that are not decision evidence", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const file = logFile(root, operationId);
  const records = readStoredRecords(file);
  const forged = structuredClone(records[records.length - 1]);
  forged.at += 1;
  forged.snapshot.lastSequence += 1;
  forged.events[0].sequence = forged.snapshot.lastSequence;
  forged.events[0].at = forged.at;
  appendStoredRecord(file, forged);
  assert.throws(
    () => reopened(root, clock),
    (error) => error instanceof OperationLogCorruptError && /same-revision commit is not decision evidence/.test(error.message),
  );
});

test("replay rejects duplicate or mutated decisions and mutable unknown outcomes", () => {
  const decisionWorld = openStore();
  const decisionId = decisionWorld.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(decisionWorld.store, decisionId);
  const waiting = decisionWorld.store.requireDecision(decisionId, "step-1", {
    decisionId: "dec-replay",
    decisionClass: "user_authorization",
    question: "Apply?",
    payloadDigest: PAYLOAD_A,
  });
  decisionWorld.store.recordDecision(recordedDecision(waiting, { decisionId: "dec-replay", payloadDigest: PAYLOAD_A }), context());
  const decisionFile = logFile(decisionWorld.root, decisionId);
  const decisionRecords = readStoredRecords(decisionFile);
  const duplicate = structuredClone(decisionRecords[decisionRecords.length - 1]);
  duplicate.events[0].sequence += 1;
  duplicate.snapshot.lastSequence += 1;
  duplicate.decisions[0].payloadDigest = PAYLOAD_B;
  fs.appendFileSync(decisionFile, `${JSON.stringify(duplicate)}\n`);
  assert.throws(() => reopened(decisionWorld.root, decisionWorld.clock), (error) => error instanceof OperationLogCorruptError);

  const outcomeWorld = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const outcomeId = outcomeWorld.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(outcomeWorld.store, outcomeId);
  outcomeWorld.store.startStep(outcomeId, "step-1", { argDigest: PAYLOAD_A });
  outcomeWorld.store.markOutcomeUnknown(outcomeId, "step-1");
  const outcomeFile = logFile(outcomeWorld.root, outcomeId);
  const outcomeRecords = readStoredRecords(outcomeFile);
  const forgedOutcome = structuredClone(outcomeRecords[outcomeRecords.length - 1]);
  forgedOutcome.snapshot.revision += 1;
  forgedOutcome.snapshot.unknownOutcomes[0].since += 1;
  forgedOutcome.events[0].sequence += 1;
  forgedOutcome.snapshot.lastSequence += 1;
  appendStoredRecord(outcomeFile, forgedOutcome);
  assert.throws(() => reopened(outcomeWorld.root, outcomeWorld.clock), (error) => error instanceof OperationLogCorruptError);
});

test("queueing persists trusted capability metadata instead of caller policy claims", () => {
  const trusted: CapabilityMetadata = {
    version: "1.0.0",
    effect: "write",
    resourceKeys: ["workspace.file:src/config.ts"],
    approval: "user",
    retry: { class: "reconcile_required", maxAttempts: 1 },
  };
  const { store } = openStore({ capabilityMetadata: () => trusted });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  const queued = store.queueStep(operationId, {
    capability: "file.edit",
  });
  assert.equal(queued.steps[0].effect, "write");
  assert.deepEqual(queued.steps[0].resourceKeys, trusted.resourceKeys);
  assert.equal(store.approvalPolicyFor("file.edit", "1.0.0"), "user");
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry: { class: "idempotent", maxAttempts: 9 } }, context()),
    (error) => isStoreError(error, "invalid_transition"),
  );
});

test("queueing refuses capabilities absent from trusted metadata", () => {
  const { store } = openStore({ capabilityMetadata: () => undefined });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  assert.throws(
    () => store.queueStep(operationId, {
      capability: "file.edit",
    }),
    (error) => isStoreError(error, "unsupported_capability"),
  );
});

test("recorded decisions must be current, not future dated, and internally consistent", () => {
  const { store, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-time",
    decisionClass: "user_authorization",
    question: "Apply it?",
    payloadDigest: PAYLOAD_A,
    ttlMs: 1_000,
  });
  const base = recordedDecision(waiting, { decisionId: "dec-time", payloadDigest: PAYLOAD_A });
  clock.advance(1_000);
  assert.throws(() => store.recordDecision(base, context()), (error) => isStoreError(error, "decision_expired"));

  const fresh = openStore();
  const freshId = fresh.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(fresh.store, freshId);
  const freshWaiting = fresh.store.requireDecision(freshId, "step-1", {
    decisionId: "dec-future",
    decisionClass: "user_authorization",
    question: "Apply it?",
    payloadDigest: PAYLOAD_A,
  });
  const future = recordedDecision(freshWaiting, {
    decisionId: "dec-future",
    payloadDigest: PAYLOAD_A,
    recordedAt: fresh.clock.now() + 1,
    expiresAt: fresh.clock.now() + 2,
  });
  assert.throws(() => fresh.store.recordDecision(future, context()), (error) => isStoreError(error, "decision_mismatch"));
  assert.throws(
    () => fresh.store.recordDecision({ ...future, recordedAt: fresh.clock.now(), expiresAt: freshWaiting.pendingDecisions[0].expiresAt, usedAt: fresh.clock.now() }, context()),
    (error) => isStoreError(error, "decision_replayed"),
  );
});

test("recording a decision rejects a stale session and a previously used record", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-session",
    decisionClass: "user_authorization",
    question: "Apply it?",
    payloadDigest: PAYLOAD_A,
  });
  const decision = recordedDecision(waiting, { decisionId: "dec-session", payloadDigest: PAYLOAD_A });
  assert.throws(
    () => store.recordDecision(decision, context({ sessionId: "s-2" })),
    (error) => isStoreError(error, "decision_mismatch"),
  );
  assert.throws(
    () => store.recordDecision({ ...decision, usedAt: decision.recordedAt }, context()),
    (error) => isStoreError(error, "decision_replayed"),
  );
});

test("additional input validates current session, expiry, and payload binding", () => {
  const { store, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "ask-bound",
    decisionClass: "additional_input",
    question: "Which target?",
    payloadDigest: canonicalDigest({ target: "src/config.ts" }),
    ttlMs: 100,
  });
  const request: ResumeRequest = {
    action: "resume",
    operationId,
    decisionId: "ask-bound",
    expectedRevision: waiting.revision,
    resolution: { kind: "additional_input", inputs: { target: "src/other.ts" } },
  };
  assert.throws(() => store.resume({ request, context: context({ sessionId: "s-2" }) }), (error) => isStoreError(error, "permission_denied"));
  assert.throws(() => store.resume({ request, context: context() }), (error) => isStoreError(error, "validation_failure"));
  clock.advance(100);
  assert.throws(
    () => store.resume({
      request: { ...request, resolution: { kind: "additional_input", inputs: { target: "src/config.ts" } } },
      context: context(),
    }),
    (error) => isStoreError(error, "decision_expired"),
  );
});

test("additional input binds and returns the complete resolution", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const resolution = {
    kind: "additional_input" as const,
    inputs: { target: "src/config.ts" },
    contextRefs: [{ kind: "message" as const, messageId: "message-2" }],
  };
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "ask-context",
    decisionClass: "additional_input",
    question: "Which target?",
    payloadDigest: canonicalDigest(resolution),
  });
  const supplied = store.resume({
    request: { action: "resume", operationId, decisionId: "ask-context", expectedRevision: waiting.revision, resolution },
    context: context(),
  });
  assert.equal(supplied.outcome, "supply_input");
  if (supplied.outcome === "supply_input") assert.deepEqual(supplied.resolution, resolution);
});

test("retry approval reuse preserves a legitimate current-session approval", () => {
  const retry = { class: "idempotent" as const, maxAttempts: 2 };
  const { store } = openStore({ capabilityMetadata: () => ({ version: "1.0.0", effect: "write", approval: "user", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-retry-binding",
    decisionClass: "user_authorization",
    question: "Apply?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-retry-binding", payloadDigest: PAYLOAD_A }), context());
  store.resume({ request: resumeRequest(operationId, "dec-retry-binding", waiting.revision, "rec-1"), context: context() });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "retry", retryable: true },
  });
  assert.doesNotThrow(() => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context()));
});

test("retry requires the current session and turn lineage", () => {
  const retry = { class: "idempotent" as const, maxAttempts: 2 };
  const { store } = openStore({ capabilityMetadata: () => ({ version: "1.0.0", effect: "write", approval: "user", retry }) });
  const operationId = store.accept({ request: instruction(), context: context({ turnRevision: 2 }) }).snapshot.operationId;
  queueEdit(store, operationId);
  const waiting = store.requireDecision(operationId, "step-1", {
    decisionId: "dec-retry-context",
    decisionClass: "user_authorization",
    question: "Apply?",
    payloadDigest: PAYLOAD_A,
  });
  store.recordDecision(recordedDecision(waiting, { decisionId: "dec-retry-context", payloadDigest: PAYLOAD_A }), context({ turnRevision: 2 }));
  store.resume({ request: resumeRequest(operationId, "dec-retry-context", waiting.revision, "rec-1"), context: context({ turnRevision: 4 }) });
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "retry", retryable: true },
  });
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context({ sessionId: "s-2", turnRevision: 4 })),
    (error) => isStoreError(error, "permission_denied"),
  );
  assert.throws(
    () => store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context({ turnRevision: 3 })),
    (error) => isStoreError(error, "revision_conflict"),
  );
});

test("retry records a fresh startedAt", () => {
  const clock = clockFrom();
  const retry = { class: "idempotent" as const, maxAttempts: 2 };
  const { store } = openStore({ clock, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none", retry }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  const started = store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A }).steps[0].startedAt;
  store.recordStepOutcome(operationId, "step-1", {
    outcome: "failed",
    error: { code: "execution_failure", message: "retry", retryable: true },
  });
  clock.advance(500);
  const retried = store.retryStep(operationId, "step-1", { argDigest: PAYLOAD_A, retry }, context());
  assert.equal(retried.snapshot.steps[0].startedAt, clock.now());
  assert.notEqual(retried.snapshot.steps[0].startedAt, started);
});

test("backend operation identity is assigned once after dispatch and survives restart", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });

  const assigned = store.recordBackendOperationId(operationId, "step-1", "backend-1");
  assert.equal(assigned.steps[0].backendOperationId, "backend-1");
  assert.equal(store.events(operationId).at(-1)?.decisionCode, "backend_operation_assigned");
  assert.throws(() => store.recordBackendOperationId(operationId, "step-1", "backend-2"), (error) => isStoreError(error, "invalid_transition"));
  assert.throws(() => store.recordBackendOperationId(operationId, "step-1", "backend-1"), (error) => isStoreError(error, "invalid_transition"));
  assert.equal(reopened(root, clock).load(operationId).steps[0].backendOperationId, "backend-1");
});

test("backend operation identity requires a running dispatched step", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  assert.throws(() => store.recordBackendOperationId(operationId, "step-1", "backend-1"), (error) => isStoreError(error, "invalid_transition"));
});

test("replay rejects backend operation identity mutation even with a forged assignment event", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordBackendOperationId(operationId, "step-1", "backend-1");
  const file = logFile(root, operationId);
  const snapshot = structuredClone(store.load(operationId));
  snapshot.revision += 1;
  snapshot.updatedAt += 1;
  snapshot.lastSequence += 1;
  snapshot.steps[0].backendOperationId = "backend-2";
  appendStoredRecord(file, {
    v: 1,
    kind: "commit",
    at: snapshot.updatedAt,
    turnRevision: 1,
    snapshot,
    events: [{ operationId, sequence: snapshot.lastSequence, at: snapshot.updatedAt, phase: "progress", revision: snapshot.revision, stepId: "step-1", summary: "forged", decisionCode: "backend_operation_assigned" }],
  });
  assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError && /backend identity changed/.test(error.message));
});

test("additional input cannot suspend a running step", () => {
  const { store } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  assert.throws(
    () => store.requireDecision(operationId, "step-1", {
      decisionId: "ask-running",
      decisionClass: "additional_input",
      question: "Which value?",
      payloadDigest: PAYLOAD_A,
    }),
    (error) => isStoreError(error, "invalid_transition"),
  );
  assert.equal(store.load(operationId).state, "running");
});

test("terminal retention preserves a durable dedupe tombstone", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  store.removeTerminalOperation(operationId);

  const restarted = reopened(root, clock, { generateOperationId: () => "op-reused" });
  const replayed = restarted.accept({ request: instruction(), context: context() });
  assert.equal(replayed.replayed, true);
  assert.equal(replayed.snapshot.operationId, operationId);
  assert.throws(
    () => restarted.accept({ request: instruction("different"), context: context() }),
    (error) => error instanceof DuplicateRequestConflictError,
  );
});

for (const failure of ["unlink", "directory-fsync"] as const) {
  test(`a ${failure} failure during terminal pruning poisons writes until reopen`, () => {
    const root = tempRoot();
    const clock = clockFrom();
    const seed = openStore({ root, clock, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
    const operationId = seed.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
    queueEdit(seed.store, operationId);
    seed.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
    seed.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });

    const store = reopened(root, clock, { fs: failingPruneFs(root, failure), capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
    assert.throws(() => store.removeTerminalOperation(operationId), new RegExp(`simulated .*${failure === "unlink" ? "unlink" : "fsync"} failure`));
    assert.throws(
      () => store.accept({ request: instruction("new work"), context: context({ toolCallId: "new-work" }) }),
      (error) => isStoreError(error, "execution_failure") && /reopen/.test((error as Error).message),
    );

    const recovered = reopened(root, clock, { capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
    assert.equal(recovered.accept({ request: instruction(), context: context() }).snapshot.operationId, operationId);
    assert.equal(fs.existsSync(logFile(root, operationId)), false);
  });
}

test("startup accepts a matching tombstone and terminal log left by a prune crash", () => {
  const root = tempRoot();
  const clock = clockFrom();
  const seed = openStore({ root, clock, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = seed.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(seed.store, operationId);
  seed.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  seed.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  const terminalLog = fs.readFileSync(logFile(root, operationId));
  seed.store.removeTerminalOperation(operationId);
  fs.writeFileSync(logFile(root, operationId), terminalLog, { mode: 0o600 });

  const recovered = reopened(root, clock);
  assert.equal(recovered.accept({ request: instruction(), context: context() }).snapshot.operationId, operationId);
  assert.equal(fs.existsSync(logFile(root, operationId)), false, "startup finishes the tombstone-authoritative prune");
});

test("startup rejects a tombstone and terminal log whose projection does not match", () => {
  const root = tempRoot();
  const clock = clockFrom();
  const seed = openStore({ root, clock, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = seed.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(seed.store, operationId);
  seed.store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  seed.store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  const records = readStoredRecords(logFile(root, operationId));
  seed.store.removeTerminalOperation(operationId);
  records[records.length - 1].snapshot.requestDigest = `sha256:${"0".repeat(64)}`;
  fs.writeFileSync(logFile(root, operationId), Buffer.concat([Buffer.from("O05v2\n"), ...records.map(encodeCommitFrame)]), { mode: 0o600 });

  assert.throws(() => reopened(root, clock), (error) => error instanceof OperationLogCorruptError);
});

test("terminal tombstones retain no original request or full snapshot", () => {
  const secret = "secret-input-that-must-not-outlive-history";
  const { store, root } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(secret), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const terminal = store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  store.removeTerminalOperation(operationId);

  const bytes = fs.readFileSync(path.join(root, "operations", "tombstones.log"), "utf8");
  assert.equal(bytes.includes(secret), false);
  assert.equal(bytes.includes('"request"'), false);
  assert.equal(bytes.includes('"snapshot"'), false);
  const retained = store.inspect(operationId, 0);
  assert.equal(retained.cursorStatus, "snapshot_required");
  assert.deepEqual(retained.retainedProjection, {
    operationId,
    revision: terminal.revision,
    state: terminal.state,
    createdAt: terminal.createdAt,
    updatedAt: terminal.updatedAt,
    lastSequence: terminal.lastSequence,
    result: terminal.result,
  });
  assert.equal(retained.snapshot, undefined);
});

test("bounded terminal pruning honors age, limit, and active operations", () => {
  const clock = clockFrom();
  const { store, root } = openStore({ clock, capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const finish = (toolCallId: string) => {
    const operationId = store.accept({ request: instruction(toolCallId), context: context({ toolCallId }) }).snapshot.operationId;
    queueEdit(store, operationId);
    store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
    store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
    return operationId;
  };
  const oldA = finish("old-a");
  clock.advance(10);
  const oldB = finish("old-b");
  clock.advance(10);
  const recent = finish("recent");
  const active = store.accept({ request: instruction("active"), context: context({ toolCallId: "active" }) }).snapshot.operationId;

  assert.deepEqual(store.pruneTerminalOperations({ before: clock.now(), limit: 1 }), [oldA]);
  assert.equal(fs.existsSync(logFile(root, oldA)), false);
  assert.equal(fs.existsSync(logFile(root, oldB)), true);
  assert.deepEqual(store.pruneTerminalOperations({ before: clock.now(), limit: 10 }), [oldB]);
  assert.equal(fs.existsSync(logFile(root, recent)), true, "the exclusive cutoff keeps an operation updated at before");
  assert.equal(fs.existsSync(logFile(root, active)), true, "active history is never selected");

  const restarted = reopened(root, clock, { generateOperationId: () => "op-new" });
  assert.equal(restarted.accept({ request: instruction("old-a"), context: context({ toolCallId: "old-a" }) }).snapshot.operationId, oldA);
  assert.throws(
    () => restarted.accept({ request: instruction("changed"), context: context({ toolCallId: "old-a" }) }),
    (error) => error instanceof DuplicateRequestConflictError,
  );
});

test("compacted terminal history exposes explicit cursor status", () => {
  const { store, root, clock } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const terminal = store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  store.removeTerminalOperation(operationId);
  const restarted = reopened(root, clock);

  assert.deepEqual(restarted.inspect(operationId, terminal.lastSequence), {
    events: [],
    decisions: [],
    cursorStatus: "caught_up",
    earliestSequence: terminal.lastSequence,
    retainedProjection: {
      operationId,
      revision: terminal.revision,
      state: terminal.state,
      createdAt: terminal.createdAt,
      updatedAt: terminal.updatedAt,
      lastSequence: terminal.lastSequence,
      result: terminal.result,
    },
  });
  assert.equal(restarted.inspect(operationId, terminal.lastSequence + 1).cursorStatus, "cursor_ahead");
  const resync = restarted.inspect(operationId, 0);
  assert.equal(resync.cursorStatus, "snapshot_required");
  assert.equal(resync.resyncSnapshot, undefined);
  assert.equal(resync.retainedProjection?.state, terminal.state);
  assert.equal(resync.earliestSequence, terminal.lastSequence);
});

test("active history reports replay and caught-up cursor status", () => {
  const { store } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.beginResolution(operationId);
  const replay = store.inspect(operationId, 0);
  assert.equal(replay.cursorStatus, "replay");
  assert.equal(replay.earliestSequence, 1);
  assert.deepEqual(replay.events.map((event) => event.sequence), [1, 2]);
  assert.equal(store.inspect(operationId, 2).cursorStatus, "caught_up");
});

test("a corrupt durable tombstone refuses recovery", () => {
  const { store, root } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  store.removeTerminalOperation(operationId);
  const index = path.join(root, "operations", "tombstones.log");
  const bytes = fs.readFileSync(index);
  bytes[bytes.length - 1] ^= 1;
  fs.writeFileSync(index, bytes);
  assert.throws(() => reopened(root, clockFrom()), (error) => error instanceof OperationLogCorruptError && /checksum/i.test(error.message));
});

test("an unterminated parseable but invalid tail is corruption", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  const file = logFile(root, operationId);
  appendStoredRecord(file, { v: 1, kind: "commit", at: clock.now(), turnRevision: 1 });
  assert.throws(
    () => reopened(root, clock),
    (error) => error instanceof OperationLogCorruptError && /snapshot/.test(error.message),
  );
});

test("event sequence cannot restart at a commit boundary", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  store.beginResolution(operationId);
  const file = logFile(root, operationId);
  const records = readStoredRecords(file) as Array<{ snapshot: OperationSnapshot; events: Array<{ sequence: number }> }>;
  const forged = JSON.parse(JSON.stringify(records[records.length - 1])) as (typeof records)[number];
  forged.snapshot.lastSequence = 3;
  forged.events[0].sequence = 2;
  appendStoredRecord(file, forged);

  assert.throws(
    () => reopened(root, clock),
    (error) => error instanceof OperationLogCorruptError && /sequence|lastSequence/i.test(error.message),
  );
});

test("an invalid complete v2 frame header is corruption, not silently skipped", () => {
  const { store, root, clock } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  const file = logFile(root, operationId);
  fs.appendFileSync(file, "\n");
  assert.throws(
    () => reopened(root, clock),
    (error) => error instanceof OperationLogCorruptError && /frame 2 has an invalid header/.test(error.message),
  );

  const spaced = openStore();
  const spacedId = spaced.store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  const spacedFile = logFile(spaced.root, spacedId);
  fs.appendFileSync(spacedFile, "   \n");
  assert.throws(
    () => reopened(spaced.root, spaced.clock),
    (error) => error instanceof OperationLogCorruptError && /frame 2 has an invalid header/.test(error.message),
  );
});

test("entries that are not operation logs are reported, not loaded", () => {
  const { root } = openStore();
  fs.mkdirSync(path.join(root, "operations"), { recursive: true });
  fs.writeFileSync(path.join(root, "operations", "notes.txt"), "not an operation");
  fs.writeFileSync(path.join(root, "operations", "op-NotAnId.jsonl"), "{}\n");
  const store = reopened(root, clockFrom());
  assert.deepEqual(store.operationIds(), []);
  assert.deepEqual([...store.ignoredEntries].sort(), ["notes.txt", "op-NotAnId.jsonl"]);
});

test("every durable write needs the exclusive bench ownership", () => {
  const { store, ownership } = openStore();
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  ownership.release();
  assert.throws(() => store.accept({ request: instruction(), context: context({ toolCallId: "call-9" }) }), (error) => error instanceof StoreNotOwnedError);
  assert.throws(() => queueEdit(store, operationId), (error) => error instanceof StoreNotOwnedError);
  assert.throws(() => store.settle(operationId), (error) => error instanceof StoreNotOwnedError);
  assert.equal(store.load(operationId).state, "accepted", "reads still see the durable state");

  const held = openStore();
  held.store.close();
  assert.throws(
    () => held.store.accept({ request: instruction(), context: context() }),
    (error) => error instanceof StoreClosedError,
  );
});

test("active operation history is not removable and terminal history is reclaimed whole", () => {
  const { store, root } = openStore({ capabilityMetadata: (capability) => ({ ...(testMetadata(capability)!), approval: "none" }) });
  const operationId = store.accept({ request: instruction(), context: context() }).snapshot.operationId;
  assert.throws(() => store.removeTerminalOperation(operationId), (error) => isStoreError(error, "invalid_transition"));
  queueEdit(store, operationId);
  store.startStep(operationId, "step-1", { argDigest: PAYLOAD_A });
  const finished = store.recordStepOutcome(operationId, "step-1", { outcome: "succeeded", evidenceRefs: ["change-1"] });
  assert.equal(finished.state, "completed");
  store.removeTerminalOperation(operationId);
  assert.deepEqual(store.operationIds(), []);
  assert.equal(fs.existsSync(logFile(root, operationId)), false);
  assert.deepEqual(reopened(root, clockFrom()).operationIds(), []);
});
