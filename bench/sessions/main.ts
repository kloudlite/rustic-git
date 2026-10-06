// Idle/readiness probe for the bench pod: no SDK, no warm sessions. Binds 127.0.0.1:8917 only —
// bench-pod-local, never exposed off the box. `--ping` is the agent's readiness probe contract
// (bins/agent/.../bench.rs:122): "is this bench quiesced enough to snapshot/stop", answered from
// the same idle tracking `/idle` exposes so the probe and the UI cannot drift apart. Idle = no
// tmux client attached.
import http from "node:http";
import { execFileSync } from "node:child_process";

const PORT = 8917;
const IDLE_SECS = Number(process.env.KL_BENCH_IDLE_SECS ?? 300);

function clients(): number {
  try {
    const out = execFileSync("tmux", ["list-clients", "-t", "kl"], { encoding: "utf8" });
    return out.split("\n").filter((l) => l.length).length;
  } catch {
    return 0;
  }
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
