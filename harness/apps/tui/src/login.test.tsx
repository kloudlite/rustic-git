import { afterAll, expect, test, mock } from "bun:test";
import * as agent from "@kloudlite-tui/agent";

// mock.module is process-wide, so put the real export back afterwards
const real = agent.loginProvider;
const ctl = new AbortController();
mock.module("@kloudlite-tui/agent", () => ({
  ...agent,
  loginProvider: (_p: string, _t: string, io: any) => {
    io.notify({ type: "progress", message: "polling for approval" });
    io.notify({
      type: "info",
      message: "Need an account?",
      links: [{ url: "https://x.test/signup", label: "Sign up" }],
    });
    // the real flow keeps running when a raced prompt is cancelled
    return io
      .prompt({ type: "manual_code", message: "Paste the code", signal: ctl.signal })
      .catch(() => new Promise(() => {}));
  },
}));
afterAll(() => {
  mock.module("@kloudlite-tui/agent", () => ({ ...agent, loginProvider: real }));
});

const { testRender } = await import("@opentui/react/test-utils");
const { Login } = await import("./components/Login.tsx");

// pi reports progress and links during a slow OAuth poll, and cancels a
// single prompt when something else answers the step (a manual_code raced
// against the callback server) — all three used to be dropped on the floor
test("login shows progress and links, and clears a cancelled prompt", async () => {
  const t = await testRender(<Login provider="anthropic" type="oauth" onDone={() => {}} />, {
    width: 100,
    height: 20,
  });
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  const before = t.captureCharFrame();
  expect(before).toContain("polling for approval");
  expect(before).toContain("https://x.test/signup");
  expect(before).toContain("Paste the code");

  ctl.abort();
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  expect(t.captureCharFrame()).not.toContain("Paste the code");
});
