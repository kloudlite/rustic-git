import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { batchImport, toItem, type ImportItem, type LooseFile } from "../../src/import-payload.ts";

async function up() {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-imp-")), readOnly: false, model: "fake/m" });
  try {
    await bench.start();
    const srv = await serve(bench, 0);
    const base = `http://127.0.0.1:${srv.port}`;
    return { bench, base, down: async () => (await bench.stop(), await srv.close()) };
  } catch (e) {
    await bench.stop();
    throw e;
  }
}
const post = async (base: string, body: unknown) => {
  const r = await fetch(base + "/import", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  return { status: r.status, body: await r.json() };
};

test("a laptop's remembered session and its loose sibling land on the bench, and importing again changes nothing", async () => {
  const t = await up();
  try {
    // The imported row uses an id the bench does not have yet.
    const items: ImportItem[] = [toItem({ id: "s-101", name: "old session", seq: 101, lastActive: 111 }, "s-101.jsonl", '{"role":"user","content":"hi"}\n')];
    const loose: LooseFile[] = [{ name: "s-9.jsonl", content: '{"role":"user","content":"orphan"}\n' }];
    const batches = batchImport(items, loose);
    assert.equal(batches.length, 1);

    const first = await post(t.base, batches[0]);
    assert.equal(first.status, 200);
    const r1 = first.body as { added: string[]; files: number };
    assert.deepEqual(r1.added, ["s-101"]);
    assert.equal(r1.files, 2);

    const rows = (await (await fetch(t.base + "/sessions")).json()) as { id: string; name: string; file?: string }[];
    const row = rows.find((x) => x.id === "s-101")!;
    assert.equal(row.name, "old session");
    assert.ok(row.file && fs.existsSync(row.file));
    assert.equal(fs.readFileSync(row.file!, "utf8"), '{"role":"user","content":"hi"}\n');
    assert.ok(fs.existsSync(path.join(path.dirname(row.file!), "s-9.jsonl")));

    // Idempotent: the same batch again adds nothing and overwrites no file.
    const second = await post(t.base, batches[0]);
    const r2 = second.body as { added: string[]; files: number };
    assert.deepEqual(r2.added, []);
    assert.equal(r2.files, 0);
    assert.equal(fs.readFileSync(row.file!, "utf8"), '{"role":"user","content":"hi"}\n');
  } finally {
    await t.down();
  }
});

test("a large import splits into batches under the server's cap, and every batch lands", async () => {
  const t = await up();
  try {
    const items: ImportItem[] = Array.from({ length: 5 }, (_, i) =>
      toItem({ id: `s-${200 + i}`, name: `s${i}`, seq: 200 + i, lastActive: i }, `s-${200 + i}.jsonl`, "x".repeat(200)),
    );
    const batches = batchImport(items, [], 500);
    assert.ok(batches.length > 1, "five 200-byte items do not fit one 500-byte batch");

    let added: string[] = [];
    for (const b of batches) {
      const r = await post(t.base, b);
      assert.equal(r.status, 200);
      added = added.concat((r.body as { added: string[] }).added);
    }
    assert.deepEqual(added.sort(), items.map((i) => i.row.id).sort());
  } finally {
    await t.down();
  }
});
