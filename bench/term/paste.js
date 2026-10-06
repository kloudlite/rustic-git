// Injected first into ttyd's page by bench/term/main.ts. Remembers ttyd's websocket, and turns a
// paste holding an image into: one `K`+bytes frame (main.ts saves it as the clipboard), then a
// Ctrl+V keystroke (ttyd INPUT `0` + 0x16), which makes the agent CLI read it through `xclip`.
// A paste with no image is left to xterm untouched.
(() => {
  const Native = window.WebSocket;
  let sock = null;
  window.WebSocket = function (url, protocols) {
    const s = new Native(url, protocols);
    sock = s;
    return s;
  };
  window.WebSocket.prototype = Native.prototype;
  Object.assign(window.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });

  const MAX = 15 << 20; // under main.ts's 16 MiB inspection limit

  window.addEventListener(
    "paste",
    async (e) => {
      const item = [...(e.clipboardData?.items ?? [])].find((i) => i.kind === "file" && i.type.startsWith("image/"));
      if (!item || !sock || sock.readyState !== 1) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      const bytes = new Uint8Array(await item.getAsFile().arrayBuffer());
      if (bytes.length > MAX) {
        console.warn(`image paste skipped: ${bytes.length} bytes is over ${MAX}`);
        return;
      }
      const frame = new Uint8Array(bytes.length + 1);
      frame[0] = 0x4b; // 'K'
      frame.set(bytes, 1);
      sock.send(frame);
      sock.send(new Uint8Array([0x30, 0x16])); // '0' INPUT, Ctrl+V
    },
    true,
  );
})();
