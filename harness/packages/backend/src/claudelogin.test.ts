import { test, expect } from "bun:test";
import { claudeLogin } from "./claudelogin";

// Shaped like the probe's output: OSC 8 hyperlink + SGR colour around the URL, prompt without newline.
const URL = "https://claude.com/cai/oauth/authorize?code=true&client_id=x&state=y";
const URL_LINE = `Opening browser to sign in…\r\nIf the browser didn't open, visit: \x1b]8;;${URL}\x07\x1b[94m${URL}\x1b[39m\x1b]8;;\x07\r\n`;
const CODE_PROMPT = "Paste code here if prompted > ";
const DONE_LINE = "Login successful."; // placeholder: the real success line was not observed

test("relays the URL, answers the code prompt, resolves on success", async () => {
  const written: string[] = [];
  const events: any[] = [];
  const fakeSpawn: any = () => {
    const out = new ReadableStream({ start(c) { const e = new TextEncoder(); c.enqueue(e.encode(`${URL_LINE}${CODE_PROMPT}`)); setTimeout(() => { c.enqueue(e.encode(`\r\n${DONE_LINE}\r\n`)); c.close(); }, 20); } });
    return { stdout: out, stdin: { write: (s: string) => void written.push(s), end() {} }, exited: Promise.resolve(0) };
  };
  await claudeLogin((e) => events.push(e), async () => " CODE123 ", fakeSpawn);
  expect(events).toEqual([{ type: "auth_url", url: URL }]);
  expect(written.join("")).toBe("CODE123\n");
});

test("rejects when claude exits non-zero", async () => {
  const fakeSpawn: any = () => ({ stdout: new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("Login failed\r\n")); c.close(); } }), stdin: { write() {} }, exited: Promise.resolve(1) });
  await expect(claudeLogin(() => {}, async () => "x", fakeSpawn)).rejects.toThrow("claude login failed");
});
