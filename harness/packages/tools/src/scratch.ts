//! The MAIN session's hands: `bash`, `read`, `write`, confined to one scratch folder. Main has no
//! workspace pod, yet codemode scripts need a shell and a place for files. The confinement keeps the
//! bench's home, the owner's keys and every other session out of reach: paths are checked against the
//! real (symlink-resolved) root, and bash runs in fresh namespaces where only that folder is visible.
//! Workspace sessions never get these; they have the pod's tools (pod.ts).
import { spawn } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import type { ToolDef } from "./index.ts";

// ponytail: folders live until the bench restarts (/tmp is an emptyDir) and nothing deletes a main
// session today; remove `scratchRoot(key)` where main sessions get dropped once that path exists.
export const scratchRoot = (key: string) => `/tmp/kl-main/${key.replace(/[^A-Za-z0-9_-]/g, "_")}`;

const READ_CAP = 256 * 1024;
const OUT_CAP = 50 * 1024;
const outside = (root: string) => `error: path outside the scratch folder (${root})`;

/** The absolute path `p` names inside `root`, or undefined when it would leave it. Relative paths
 * resolve against root. The nearest EXISTING ancestor is realpath'd (so `..` and symlinked parents
 * are caught before anything is created), and the target too when it exists (a symlink inside root
 * pointing out must not be followed). */
export function confine(root: string, p: string): string | undefined {
  const real = realpathSync(root);
  const abs = resolve(root, p);
  let anc = abs;
  while (!lexists(anc) && dirname(anc) !== anc) anc = dirname(anc);
  let ra: string;
  try {
    ra = realpathSync(anc); // throws on a dangling symlink: refuse, writing would follow it out
  } catch {
    return undefined;
  }
  return ra === real || ra.startsWith(real + "/") ? abs : undefined;
}
const lexists = (p: string) => {
  try {
    lstatSync(p);
    return true;
  } catch {
    return false;
  }
};

// Runs as root in new user+mount+pid namespaces (proven on the bench under gVisor 2026-10-08).
// `-n` everywhere: without it `mount --move` fails "failed to update userspace mount table".
// Only $R survives from the real filesystem view; /tmp, the home and the keys dir are blanked.
// The INNER `unshare -Um --map-user` is LOAD-BEARING: as namespace root the script could
// `umount /home/kl` and see the real home again; as uid 1000 it cannot (proven).
// /home/kl and /etc/kloudlite are hidden only where they exist, so a laptop or CI Linux works.
const OUTER = `set -e
mount -n -t tmpfs t /mnt; mkdir /mnt/s; mount -n --bind "$R" /mnt/s
mount -n -t tmpfs t /tmp; mkdir -p "$R"; mount -n --move /mnt/s "$R"; umount -n /mnt
[ -d /home/kl ] && mount -n -t tmpfs t /home/kl
[ -d /etc/kloudlite ] && mount -n -t tmpfs t /etc/kloudlite
cd "$R"
exec unshare -Um --map-user=1000 --map-group=1000 sh -c "$1"`;

type Bash = { output: string; exit_code: number };
const unavailable = (why: string): Bash => ({ output: `error: bash sandbox unavailable: ${why}`, exit_code: 1 });

function runBash(root: string, command: string, seconds: number): Promise<Bash> {
  return new Promise((done) => {
    // Nothing else: no KL_*, no tokens, no provider keys.
    const env = { PATH: process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin", HOME: root, LANG: "C.UTF-8", R: root };
    const child = spawn("unshare", ["-Urmpf", "--mount-proc", "sh", "-c", OUTER, "kl-scratch", command], { env, detached: true });
    let buf = Buffer.alloc(0);
    let cut = false;
    const take = (c: Buffer) => {
      buf = Buffer.concat([buf, c]);
      if (buf.length > OUT_CAP * 2) {
        buf = buf.subarray(buf.length - OUT_CAP);
        cut = true;
      }
    };
    child.stdout.on("data", take);
    child.stderr.on("data", take);
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        process.kill(-child.pid!, "SIGKILL");
      } catch {}
    }, seconds * 1000);
    child.on("error", (e: any) => {
      clearTimeout(timer);
      done(unavailable(e?.code === "ENOENT" ? "unshare not found" : String(e?.message ?? e)));
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (buf.length > OUT_CAP) {
        buf = buf.subarray(buf.length - OUT_CAP);
        cut = true;
      }
      let output = buf.toString("utf8");
      if (!timedOut && code !== 0 && /^(unshare|mount):/.test(output)) return done(unavailable(output.trim().split("\n")[0]!));
      if (cut) output = `[truncated]\n${output}`;
      if (timedOut) return done({ output: `${output}\n[timed out after ${seconds}s]`, exit_code: 124 });
      done({ output, exit_code: code ?? 1 });
    });
  });
}

export function scratchTools(root: string): ToolDef[] {
  mkdirSync(root, { recursive: true });
  return [
    {
      name: "bash",
      description:
        `Run a shell command in your scratch folder (${root}). This is the only part of the filesystem that the command can see. The network works. No credentials are present. ` +
        "It returns { output, exit_code }. The output is stdout and stderr. It has the last 50 KiB.",
      inputSchema: {
        type: "object",
        properties: {
          command: { type: "string" },
          timeout: { type: "number", description: "In seconds. The default is 120. The maximum is 600." },
        },
        required: ["command"],
      },
      run: async (a: { command: string; timeout?: number }) => {
        const secs = Math.min(Math.max(Number(a.timeout) || 120, 1), 600);
        return JSON.stringify(await runBash(root, String(a.command ?? ""), secs), null, 2);
      },
    },
    {
      name: "read",
      description: `Read a text file in your scratch folder (${root}). A relative path starts in this folder. The maximum size is 256 KiB. \`offset\` and \`limit\` are in lines.`,
      inputSchema: {
        type: "object",
        properties: { path: { type: "string" }, offset: { type: "number" }, limit: { type: "number" } },
        required: ["path"],
      },
      run: async (a: { path: string; offset?: number; limit?: number }) => {
        const p = confine(root, a.path);
        if (!p) return outside(root);
        if (!existsSync(p)) return `error: no such file: ${a.path}`;
        try {
          let text = readFileSync(p, "utf8");
          if (a.offset || a.limit) {
            const from = Math.max(0, (a.offset ?? 1) - 1);
            text = text.split("\n").slice(from, a.limit ? from + a.limit : undefined).join("\n");
          }
          if (Buffer.byteLength(text) <= READ_CAP) return text;
          return `${Buffer.from(text).subarray(0, READ_CAP).toString("utf8")}\n[truncated at 256 KiB]`;
        } catch (e: any) {
          return `error: ${e?.message ?? e}`;
        }
      },
    },
    {
      name: "write",
      description: `Write a text file in your scratch folder (${root}). A relative path starts in this folder. The tool creates the parent folders.`,
      inputSchema: {
        type: "object",
        properties: { path: { type: "string" }, content: { type: "string" } },
        required: ["path", "content"],
      },
      run: async (a: { path: string; content: string }) => {
        const p = confine(root, a.path);
        if (!p) return outside(root);
        try {
          mkdirSync(dirname(p), { recursive: true });
          writeFileSync(p, a.content);
          return `wrote ${Buffer.byteLength(a.content)} bytes to ${relative(resolve(root), p).replace(/^\.\..*/, "") || a.path}`;
        } catch (e: any) {
          return `error: ${e?.message ?? e}`;
        }
      },
    },
  ];
}
