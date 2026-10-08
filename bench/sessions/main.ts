// Idle/readiness probe for the bench pod: no SDK, no warm sessions. Binds 127.0.0.1:8917 only —
// bench-pod-local, never exposed off the box. `--ping` is the agent's readiness probe contract
// (bins/agent/.../bench.rs:122): "is this bench quiesced enough to snapshot/stop", answered from
// the same idle tracking `/idle` exposes so the probe and the UI cannot drift apart. Idle = no
// TUI and no laptop-TUI host running: ttyd and sshd start one per login and end it with the login.
import http from "node:http";
import fs from "node:fs";

const PORT = 8917;
const IDLE_SECS = Number(process.env.KL_BENCH_IDLE_SECS ?? 300);

const TUI = "/opt/kl/harness/apps/tui";
const HOST = "/opt/kl/harness/packages/backend/src/serve.ts";

function clients(): number {
  let n = 0;
  let pids: string[];
  try {
    pids = fs.readdirSync("/proc");
  } catch {
    return 0; // no procfs (a laptop run): nothing to count
  }
  for (const pid of pids) {
    if (!/^\d+$/.test(pid)) continue;
    try {
      // `bun run --cwd TUI dev` (ttyd, ssh) or `bun run serve.ts` (kl-host, the laptop TUI); the
      // argv[0] check skips ttyd, whose own argv carries the same command line.
      const argv = fs.readFileSync(`/proc/${pid}/cmdline`, "utf8").split("\0");
      if (argv[0].endsWith("bun") && (argv.includes(TUI) || argv.includes(HOST))) n++;
    } catch {
      // exited between readdir and read
    }
  }
  return n;
}

// Sampled every 5s rather than on every request: cheap, and `idleSince` only needs to be accurate
// to within that window for the idle-shutdown probe to behave.
let idleSince: number | null = null;
setInterval(() => {
  const isIdle = clients() === 0;
  if (isIdle) {
    if (idleSince === null) idleSince = Date.now();
  } else {
    idleSince = null;
  }
}, 5000);

function idleBody() {
  return { clients: clients(), idleSince };
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", "http://x");

  if (url.pathname === "/idle") {
    res.end(JSON.stringify(idleBody()));
    return;
  }

  res.statusCode = 404;
  res.end();
});

function ping() {
  const req = http.get(`http://127.0.0.1:${PORT}/idle`, (res) => {
    let body = "";
    res.on("data", (c) => (body += c));
    res.on("end", () => {
      try {
        const { idleSince: since } = JSON.parse(body);
        const idle = since !== null && Date.now() - since >= IDLE_SECS * 1000;
        process.exit(idle ? 1 : 0);
      } catch {
        process.exit(2);
      }
    });
  });
  req.on("error", () => process.exit(2));
}

if (process.argv.includes("--ping")) {
  ping();
} else {
  server.listen(PORT, "127.0.0.1", () => console.log(`kl-sessions on ${PORT}`));
}
