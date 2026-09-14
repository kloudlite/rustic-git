import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createStore, NoKeychain } from "../../src/auth/store.ts";

// Reversible and visibly not plaintext: a file holding the token verbatim fails the test.
const fake = (available = true) => ({
  isEncryptionAvailable: () => available,
  encryptString: (s: string) => Buffer.from(s, "utf8").map((b) => b ^ 0x5a) as Buffer,
  decryptString: (b: Buffer) => Buffer.from(b.map((x) => x ^ 0x5a)).toString("utf8"),
});
const cred = { api: "https://k.test", token: "aaa.bbb.ccc", expiresAt: "2030-01-01T00:00:00Z", username: "karthik" };
const tmp = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "desk-store-")), "credential.bin");

test("a saved credential round-trips, is 0600, and is not plaintext on disk", () => {
  const file = tmp();
  const s = createStore(file, fake());
  s.save(cred);
  assert.deepEqual(s.load(), cred);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.ok(!fs.readFileSync(file).toString("latin1").includes(cred.token));
});

test("nothing stored loads as undefined; clear removes the file and is idempotent", () => {
  const file = tmp();
  const s = createStore(file, fake());
  assert.equal(s.load(), undefined);
  s.save(cred);
  s.clear();
  s.clear();
  assert.equal(fs.existsSync(file), false);
  assert.equal(s.load(), undefined);
});

test("no keychain: save and load refuse, and no file is written", () => {
  const file = tmp();
  const s = createStore(file, fake(false));
  assert.throws(() => s.save(cred), NoKeychain);
  assert.throws(() => s.load(), NoKeychain);
  assert.equal(fs.existsSync(file), false);
});

test("an undecryptable or malformed file loads as undefined rather than throwing", () => {
  const file = tmp();
  fs.writeFileSync(file, "garbage");
  assert.equal(createStore(file, fake()).load(), undefined);
});
