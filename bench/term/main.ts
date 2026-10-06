// The browser terminal's front door on BENCH_TERM_PORT (7681): everything goes to ttyd on
// 127.0.0.1:7682 unchanged, except two things that let a person paste an IMAGE from their own
// machine's clipboard into the agent CLI.
//
// The CLI reads an image paste by running `xclip -selection clipboard -t image/png -o` on the box
// it runs on, and this box has no clipboard of its own: the image lives in the person's browser.
// So the page ttyd serves gets `paste.js` injected; on a paste holding an image it sends the bytes
// as one binary websocket frame tagged `K` (ttyd's own client tags are `0`-`3` and `{`), then a
// plain Ctrl+V. This front swallows the `K` frame, writes it to CLIP, and the `xclip` shim
// (bench/term/xclip) hands CLIP to the CLI. Frames stay in order on the socket and the write is
// synchronous, so the file exists before ttyd ever sees the Ctrl+V.
//
// In-band on ttyd's own `/ws`, not a POST: the gateway (bins/gateway/src/term.rs) proxies GETs and
// that one websocket only, and riding the socket means the gateway's auth covers the upload too.
import http from "node:http";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";

const PORT = 7681;
const TTYD = { host: "127.0.0.1", port: 7682 };
const CLIP = "/tmp/kl-clip/latest";
const CLIP_TAG = 0x4b; // 'K'
// A clipboard screenshot is a few MiB; anything this big is forwarded uninspected rather than buffered.
const MAX_CLIP = 16 << 20;

const PASTE_JS = fs.readFileSync(new URL("./paste.js", import.meta.url), "utf8");
fs.mkdirSync(path.dirname(CLIP), { recursive: true, mode: 0o700 });

function saveClip(bytes: Buffer) {
  fs.writeFileSync(CLIP + ".part", bytes, { mode: 0o600 });
  fs.renameSync(CLIP + ".part", CLIP);
}

// Client-to-server websocket frames, one at a time: a whole `K` frame is saved and dropped, every
// other frame is passed through byte for byte (still masked; ttyd unmasks it). Returns the bytes
// that do not yet make a whole frame, or null once filtering has stopped for this socket.
function filterFrames(buf: Buffer, out: net.Socket): Buffer | null {
  for (;;) {
    if (buf.length < 2) return buf;
    let len = buf[1] & 0x7f;
    let off = 2;
    if (len === 126) {
      if (buf.length < 4) return buf;
      len = buf.readUInt16BE(2);
      off = 4;
    } else if (len === 127) {
      if (buf.length < 10) return buf;
      const big = buf.readBigUInt64BE(2);
      len = big > BigInt(MAX_CLIP) ? MAX_CLIP + 1 : Number(big);
      off = 10;
    }
    const masked = (buf[1] & 0x80) !== 0;
    const hdr = off + (masked ? 4 : 0);
    if (len > MAX_CLIP) {
      // Too big to be ours: stop filtering this socket and pipe the rest straight through.
      out.write(buf);
      return null;
    }
    if (buf.length < hdr + len) return buf;
    const frame = buf.subarray(0, hdr + len);
    buf = buf.subarray(hdr + len);
    const binaryFin = (frame[0] & 0x8f) === 0x82; // FIN + binary opcode
    if (binaryFin && len > 0) {
      const payload = Buffer.from(frame.subarray(hdr));
      if (masked) for (let i = 0; i < payload.length; i++) payload[i] ^= frame[off + (i & 3)];
      if (payload[0] === CLIP_TAG) {
        try {
          saveClip(payload.subarray(1));
        } catch (e) {
          console.error("clip.save.failed", e);
        }
        continue;
      }
    }
    out.write(frame);
  }
}

const server = http.createServer((req, res) => {
  const isIndex = req.method === "GET" && (req.url === "/" || req.url?.startsWith("/?"));
  const headers = { ...req.headers };
  // ttyd gunzips its embedded page for a client that does not take gzip, so the inject sees HTML.
  if (isIndex) headers["accept-encoding"] = "identity";
  const up = http.request({ ...TTYD, method: req.method, path: req.url, headers }, (ur) => {
    if (!isIndex || ur.statusCode !== 200) {
      res.writeHead(ur.statusCode ?? 502, ur.headers);
      ur.pipe(res);
      return;
    }
    const chunks: Buffer[] = [];
    ur.on("data", (c) => chunks.push(c));
    ur.on("end", () => {
      // First thing in <head>: it must wrap WebSocket before ttyd's bundle opens its socket.
      const html = Buffer.concat(chunks).toString("utf8").replace("<head>", `<head><script>${PASTE_JS}</script>`);
      const body = Buffer.from(html);
      const h = { ...ur.headers, "content-length": String(body.length) };
      delete h["content-encoding"];
      delete h["transfer-encoding"];
      res.writeHead(200, h);
      res.end(body);
    });
  });
  up.on("error", () => {
    res.writeHead(502).end();
  });
  req.pipe(up);
});

// Websocket: replay the upgrade request to ttyd raw, pipe ttyd's side straight back, and run the
// browser's side through filterFrames.
server.on("upgrade", (req, sock, head) => {
  const up = net.connect(TTYD, () => {
    let raw = `${req.method} ${req.url} HTTP/1.1\r\n`;
    for (let i = 0; i < req.rawHeaders.length; i += 2) {
      // No permessage-deflate: browsers offer it, ttyd accepts it, and a compressed `K` frame no
      // longer starts with `K`, so every image paste went to ttyd as an unknown command.
      if (req.rawHeaders[i].toLowerCase() === "sec-websocket-extensions") continue;
      raw += `${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}\r\n`;
    }
    up.write(raw + "\r\n");
    let pending: Buffer | null = Buffer.alloc(0);
    const feed = (c: Buffer) => {
      if (pending === null) return void up.write(c);
      pending = filterFrames(Buffer.concat([pending, c]), up);
    };
    if (head.length) feed(head);
    sock.on("data", feed);
    up.pipe(sock);
  });
  const close = () => {
    sock.destroy();
    up.destroy();
  };
  up.on("error", close);
  sock.on("error", close);
  up.on("close", close);
  sock.on("close", close);
});

server.listen(PORT, "0.0.0.0");
