import { afterAll, expect, test } from "bun:test";
import { backend, boot, hello } from "./hello.ts";

// Login goes through backend().auth; re-boot with a fake and put the real one back afterwards.
const realBackend = backend();
const real = hello();
const ctl = new AbortController();
boot(
  {
    auth: {
      claudeSignedIn: async () => false,
      login: async (_p: string, _t: any, io: any) => {
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
    },
  } as any,
  real,
);
afterAll(() => boot(realBackend, real));

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
