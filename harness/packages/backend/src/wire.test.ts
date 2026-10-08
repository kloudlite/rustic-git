import { expect, test } from "bun:test";
import { encode, Peer } from "./wire.ts";

const enc = new TextEncoder();
/** Two peers wired back to back; delivery is async like a pipe. */
function pair() {
  const a: Peer = new Peer((l) => queueMicrotask(() => b.feed(enc.encode(l))));
  const b: Peer = new Peer((l) => queueMicrotask(() => a.feed(enc.encode(l))));
  return { a, b };
}

test("request round trip both directions", async () => {
  const { a, b } = pair();
  b.handle("add", async ([x, y]) => x + y);
  a.handle("echo", async (s) => s);
  expect(await a.request<number>("add", [2, 3])).toBe(5);
  expect(await b.request<string>("echo", "hi")).toBe("hi");
});

test("handler error rejects with its message", async () => {
  const { a, b } = pair();
  b.handle("boom", async () => { throw new Error("nope"); });
  await expect(a.request("boom", null)).rejects.toThrow("nope");
});

test("unknown op rejects", async () => {
  const { a } = pair();
  await expect(a.request("missing", null)).rejects.toThrow("unknown op: missing");
});

test("cancel aborts the receiver's handler signal", async () => {
  const { a, b } = pair();
  let aborted = false;
  b.handle("wait", (_x, signal) => new Promise((r) => signal.addEventListener("abort", () => { aborted = true; r(null); })));
  const ac = new AbortController();
  const p = a.request("wait", null, ac.signal);
  await Bun.sleep(5);
  ac.abort();
  await expect(p).rejects.toThrow();
  await Bun.sleep(5);
  expect(aborted).toBe(true);
});

test("events reach listeners", async () => {
  const { a, b } = pair();
  const got: unknown[] = [];
  a.onEvent((ev, key, e) => got.push([ev, key, e]));
  b.emit("session", "k", { type: "x" });
  await Bun.sleep(5);
  expect(got).toEqual([["session", "k", { type: "x" }]]);
});

test("serialises Error", () => {
  expect(JSON.parse(encode({ e: new TypeError("bad") }))).toEqual({ e: { name: "TypeError", message: "bad" } });
});

test("reassembles a line split inside a multi-byte character", () => {
  const got: unknown[] = [];
  const p = new Peer(() => {});
  p.onEvent((_ev, _k, e) => got.push(e));
  const bytes = enc.encode(encode({ ev: "session", key: "k", event: "héllo ✓" }));
  const cut = bytes.indexOf(0xc3) + 1; // inside "é"
  p.feed(bytes.slice(0, cut));
  p.feed(bytes.slice(cut));
  expect(got).toEqual(["héllo ✓"]);
});

test("carries a 4 MB line", () => {
  const got: string[] = [];
  const p = new Peer(() => {});
  p.onEvent((_ev, _k, e) => got.push(e as string));
  const big = "x".repeat(4 << 20);
  const bytes = enc.encode(encode({ ev: "session", key: "k", event: big }));
  for (let i = 0; i < bytes.length; i += 65536) p.feed(bytes.slice(i, i + 65536));
  expect(got[0]!.length).toBe(4 << 20);
});

test("skips a non-JSON line", () => {
  const got: unknown[] = [];
  const p = new Peer(() => {});
  p.onEvent((_ev, _k, e) => got.push(e));
  p.feed(enc.encode("Debugger attached.\n" + encode({ ev: "session", key: "k", event: 1 })));
  expect(got).toEqual([1]);
});

test("close rejects pending and refuses new requests", async () => {
  const p = new Peer(() => {});
  const pending = p.request("x", null);
  p.close();
  await expect(pending).rejects.toThrow("disconnected");
  await expect(p.request("y", null)).rejects.toThrow("disconnected");
});

test("a reply for a cancelled request is ignored", async () => {
  const { a, b } = pair();
  let release!: () => void;
  b.handle("slow", () => new Promise((r) => (release = () => r("late"))));
  const ac = new AbortController();
  const p = a.request("slow", null, ac.signal);
  await Bun.sleep(5);
  ac.abort();
  await expect(p).rejects.toThrow();
  release();
  await Bun.sleep(5); // must not throw on the stray reply
});
