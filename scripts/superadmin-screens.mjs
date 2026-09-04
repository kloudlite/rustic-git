#!/usr/bin/env node
// Screenshots every superadmin route at 1440 wide into .local/screens/ for the reviewer.
//
// Headless Chrome's own CLI rather than a driver library: the repo has no browser automation
// dependency and this needs none — one screenshot per URL, no interaction. Exit 77 when no
// Chrome is installed, matching tests/registry_e2e.sh's "skipped, not passed" convention.
//
// The superadmin area is behind `requireSuperadmin`, and headless Chrome's screenshot mode has no
// way to carry a cookie — so this mints the real Auth.js session cookie with AUTH_SECRET (the
// same encode the app's own sign-in uses) and serves the dev server through a small origin proxy
// that attaches it. Nothing in the app is bypassed: the app sees an ordinary signed-in session,
// and the cookie is unmintable without the deployment secret.
//
// Usage (a laptop with no cluster):
//   cd web && KLOUDLITE_GIT_ADMIN_FIXTURES=1 AUTH_SECRET=dev-secret AUTH_URL=http://localhost:3000 \
//     bun run dev
//   AUTH_SECRET=dev-secret node scripts/superadmin-screens.mjs [http://localhost:3000]
//
// `KLOUDLITE_GIT_ADMIN_FIXTURES=1` is what makes the pages render without an admin API: every GET the
// console makes is answered from web/apps/web/src/lib/fixtures/superadmin.ts. Writes are not faked,
// so the forms on these screens still need a real cluster.
import { spawn } from "node:child_process";
import { createServer, request as httpRequest } from "node:http";
import { existsSync, mkdirSync, globSync, rmSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const BASE = process.argv[2] ?? "http://localhost:3000";
const OUT = resolve(REPO, ".local/screens");
const SECRET = process.env.AUTH_SECRET ?? "";
/** Whoever the local session claims to be. Only the claim matters — every page reads its data
 *  from the fixtures, and every write still goes to a real api that re-checks the claim itself. */
const USER = { email: process.env.SCREENS_EMAIL ?? "karthik@kloudlite.io", username: "karthik" };

const ROUTES = [
  ["overview", "/superadmin"],
  ["requests", "/superadmin/requests"],
  ["owners", "/superadmin/owners"],
  ["owner", "/superadmin/owners/acme"],
  ["clusters", "/superadmin/clusters"],
  ["cluster", "/superadmin/clusters/centralindia-k3s"],
  ["monitoring", "/superadmin/monitoring"],
  ["audit", "/superadmin/audit"],
  ["access", "/superadmin/access"],
  ["configuration", "/superadmin/configuration"],
];

const CHROMES = [
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
];
const chrome = CHROMES.find(existsSync);
if (!chrome) {
  console.error("no Chrome or Chromium found; install one to take screenshots");
  process.exit(77);
}
if (!SECRET) {
  console.error("AUTH_SECRET is required: it is what the session cookie is encrypted with, and it must match the dev server's");
  process.exit(1);
}

/** Auth.js is installed under bun's isolated layout, so `@auth/core` is not resolvable from the
 *  repo root — resolve it the way next-auth itself does, from next-auth's own directory. */
async function authJwt() {
  const [pkg] = globSync("web/node_modules/.bun/next-auth@*/node_modules/next-auth/package.json", { cwd: REPO });
  if (!pkg) throw new Error("next-auth is not installed; run `cd web && bun install`");
  const req = createRequire(resolve(REPO, pkg));
  return import(pathToFileURL(req.resolve("@auth/core/jwt")).href);
}

/** The same claims `auth.ts`'s jwt callback mints at sign-in. `apiToken` is a placeholder: under
 *  fixtures no call reaches an api that would check it, and a real one would need a real cluster. */
async function sessionCookie() {
  const { encode } = await authJwt();
  const name = "authjs.session-token"; // no `__Secure-` prefix: local dev is http, per auth.ts
  const token = await encode({
    salt: name,
    secret: SECRET,
    maxAge: 12 * 60 * 60,
    token: {
      sub: USER.email,
      email: USER.email,
      name: USER.username,
      username: USER.username,
      superadmin: true,
      apiToken: "fixtures",
    },
  });
  return `${name}=${token}`;
}

/** An origin proxy, not an HTTP proxy: Chrome is pointed straight at it, so every relative URL the
 *  app emits comes back here and carries the session. */
function startProxy(cookie) {
  const target = new URL(BASE);
  const server = createServer((req, res) => {
    const upstream = httpRequest(
      {
        host: target.hostname,
        port: target.port || 80,
        path: req.url,
        method: req.method,
        headers: { ...req.headers, host: target.host, cookie },
      },
      (up) => {
        res.writeHead(up.statusCode ?? 502, up.headers);
        up.pipe(res);
      },
    );
    upstream.on("error", (e) => {
      res.writeHead(502).end(`dev server unreachable: ${e.message}`);
    });
    req.pipe(upstream);
  });
  return new Promise((ok) => server.listen(0, "127.0.0.1", () => ok(server)));
}

/** Resolves once the file exists and has stopped growing — Chrome writes the PNG in one go, but
 *  the size check keeps a half-written frame out of the review. */
async function waitForFile(file, ms) {
  const deadline = Date.now() + ms;
  let last = -1;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 500));
    const size = existsSync(file) ? statSync(file).size : -1;
    if (size > 0 && size === last) return true;
    last = size;
  }
  return false;
}

const server = await startProxy(await sessionCookie());
const origin = `http://127.0.0.1:${server.address().port}`;
mkdirSync(OUT, { recursive: true });
let failed = 0;
for (const [name, path] of ROUTES) {
  const file = `${OUT}/${name}.png`;
  rmSync(file, { force: true });
  const child = spawn(
    chrome,
    [
      "--headless=new",
      "--disable-gpu",
      "--hide-scrollbars",
      "--no-first-run",
      // Its own profile: a Chrome the reviewer already has open holds a lock on the default one.
      `--user-data-dir=${OUT}/.chrome`,
      // 1440 is the artboards' own width, so a screenshot lines up with the mockup beside it.
      "--window-size=1440,2400",
      // The dev server compiles a route on first hit; give it the compile.
      "--virtual-time-budget=15000",
      `--screenshot=${file}`,
      `${origin}${path}`,
    ],
    { stdio: "ignore" },
  );
  // Chrome writes the PNG and then does NOT exit: these pages poll every 10 s and dev mode holds
  // an HMR socket open, so nothing ever quiesces. Waiting on the FILE rather than on the process
  // is what makes this terminate — the shot is on disk long before the browser gives up.
  const ok = await waitForFile(file, 60_000);
  child.kill("SIGKILL");
  if (!ok) {
    console.error(`${name} ✗ no screenshot after 60s (${path})`);
    failed++;
    continue;
  }
  console.log(`${name} → ${file}`);
}
server.close();
process.exit(failed > 0 ? 1 : 0);
