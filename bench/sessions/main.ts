// HTTP front door for the warm-session state machine in sessions.ts. Binds 127.0.0.1:8917 only —
// this is a bench-pod-local service, never exposed off the box. `--ping` is the agent's readiness
// probe contract (bins/agent/.../bench.rs:122): it needs a cheap, idempotent way to ask "is this
// bench quiesced enough to snapshot/stop", answered from the same idle tracking `/idle` exposes so
// there's one source of truth for "idle" instead of the probe and the UI drifting apart.
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { query } from "@anthropic-ai/claude-agent-sdk";
import { Sessions } from "./sessions.ts";

const PORT = 8917;
const HOME = process.env.HOME ?? "/home/kl";
const MOD_DIR = process.env.KL_MOD_DIR ?? "/opt/kl/mod";
const IDLE_SECS = Number(process.env.KL_BENCH_IDLE_SECS ?? 300);

// ponytail: single JSON file, whole-write on each turn end; sqlite if it ever grows
const STORE = path.join(HOME, "sessions", "sessions.json");
let saved: Record<string, unknown> = {};
try {
  saved = JSON.parse(fs.readFileSync(STORE, "utf8"));
} catch {
  // first boot, or a corrupt/missing store: start from nothing rather than crash the service
}

const sessions = new Sessions({ query: query as any, home: HOME, modDir: MOD_DIR, saved: saved as any });

function persist() {
  fs.mkdirSync(path.dirname(STORE), { recursive: true });
  fs.writeFileSync(STORE, JSON.stringify(sessions.toJSON()));
}
// Persist on a slow beat rather than on every single SDK message; a turn-granular write (every
// `result`) is plenty durable for a bench that can be rebuilt from the workspace anyway.
setInterval(persist, 5000);

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
  const isIdle = clients() === 0 && !sessions.busy();
  if (isIdle) {
    if (idleSince === null) idleSince = Date.now();
  } else {
    idleSince = null;
  }
}, 5000);

function idleBody() {
  return { clients: clients(), busy: sessions.busy(), idleSince };
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", "http://x");

  if (req.method === "POST" && url.pathname === "/send") {
    let body = "";
    for await (const c of req) body += c;
    const { ws, text, agentId } = JSON.parse(body);
    sessions.send(ws, text, agentId);
    res.end("{}");
    return;
  }

  if (url.pathname === "/state") {
    res.end(JSON.stringify(sessions.state()));
    return;
  }

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
