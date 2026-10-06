// Injected first into ttyd's page by bench/term/main.ts. Remembers ttyd's websocket, and turns a
// paste holding an image into: one `K`+bytes frame (main.ts saves it as the clipboard), then a
// Ctrl+V keystroke (ttyd INPUT `0` + 0x16), which makes the agent CLI read it through `xclip`.
// A paste with no image is left to xterm untouched.
(() => {
  // IBM Plex Mono for the terminal (ttyd's `-t fontFamily`, bench/sv/ttyd/run). xterm measures the
  // cell size once, before a webfont arrives, so re-set the family once it has loaded: that makes
  // xterm re-measure, and the resize event makes ttyd refit the grid.
  const font = document.createElement("link");
  font.rel = "stylesheet";
  font.href = "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:ital,wght@0,400;0,600;1,400&display=swap";
  document.head.appendChild(font);
  // Edge to edge: ttyd pads the terminal 5px; the TUI should own the whole window.
  const fill = document.createElement("style");
  fill.textContent = "html,body,#terminal-container{width:100%;height:100%;margin:0;padding:0}.terminal{padding:0!important;height:100%!important}";
  document.head.appendChild(fill);
  document.fonts.load('16px "IBM Plex Mono"').then(() => {
    const term = window.term;
    if (!term) return;
    // Two different values: setting the same family again is a no-op and measures nothing.
    term.options.fontFamily = "monospace";
    term.options.fontFamily = '"IBM Plex Mono", monospace';
    window.dispatchEvent(new Event("resize"));
  });

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
