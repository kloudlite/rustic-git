// One in-process handler standing in for `kl ide serve`: a map of files, a scripted exec.
import http from "node:http";

export type Fake = { address: string; files: Map<string, string>; execs: string[]; exec: (cmd: string) => { exit_code: number; stdout: string; stderr: string }; close: () => void };

export async function fakeTools(exec: Fake["exec"] = () => ({ exit_code: 0, stdout: "", stderr: "" })): Promise<Fake> {
  const files = new Map<string, string>();
  const execs: string[] = [];
  const srv = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const name = req.url!.replace("/tools/", "");
      const a = body ? JSON.parse(body) : {};
      const ok = (v: unknown) => res.end(JSON.stringify(v));
      const err = (code: number, m: string) => { res.statusCode = code; res.end(JSON.stringify({ error: m })); };
      if (req.method === "GET" && req.url === "/tools") return ok({ tools: [...new Set(["read", "write", "edit", "glob", "grep", "exec", "process_output", "process_kill"])].map((n) => ({ name: n })) });
      switch (name) {
        case "read": { const p = a.paths?.[0] ?? a.path; return files.has(p) ? ok({ path: p, content: files.get(p), total_lines: files.get(p)!.split("\n").length }) : err(404, `no such file: ${p}`); }
        case "write": files.set(a.path, a.content); return ok({ path: a.path, bytes: a.content.length });
        case "edit": { const f = a.files[0]; const cur = files.get(f.path) ?? ""; if (!cur.includes(f.edits[0].old)) return err(400, "old not found"); files.set(f.path, cur.replace(f.edits[0].old, f.edits[0].new)); return ok({ path: f.path, applied: 1 }); }
        case "glob": return ok({ cwd: ".", paths: [...files.keys()], truncated: false });
        case "grep": return ok({ matches: [...files].flatMap(([p, c]) => c.split("\n").map((t, i) => ({ path: p, line: i + 1, text: t })).filter((m) => m.text.includes(a.pattern))) });
        case "exec": execs.push(a.cmd); return ok(fake.exec(a.cmd));
        case "process_output": return ok({ stdout: "", stderr: "", next: 0 });
        case "process_kill": return ok({ ok: true });
        default: return err(404, `no tool ${name}`);
      }
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const fake: Fake = { address: `127.0.0.1:${(srv.address() as { port: number }).port}`, files, execs, exec, close: () => srv.close() };
  return fake;
}
