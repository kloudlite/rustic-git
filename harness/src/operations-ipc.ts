/**
 * The pure pieces of the `operations:*` IPC surface — kept free of `electron` so `node:test`
 * can import this module directly (`main.ts` pulls in `electron` at module load, which node
 * cannot resolve outside the Electron runtime). `main.ts` imports this and wires it to the real
 * `ipcMain`/`BrowserWindow`.
 */

/**
 * The guard only ever compares these by identity (`===`), never reads through them, so it takes
 * `unknown` rather than importing Electron's `WebFrameMain` type — that is what keeps this
 * module free of an `electron` import for `node:test`.
 */
export type SenderFrame = unknown;
export type MainFrameHandle = unknown;

/**
 * `outcome: "granted"` is a grant with a real effect and no native confirmation (ruling 5, no
 * dialog is added); the only thing standing between a compromised child frame — Chat renders
 * model HTML, and the CSP is the other half of this defence — and a forged approval is checking
 * the call came from the window's own top-level document, not an iframe or devtools target.
 * `e.senderFrame` is undefined once the frame that sent the call is already gone, which reads
 * the same as "not the main frame": there is nothing left to trust it with.
 */
export function isMainFrame(senderFrame: SenderFrame, mainFrame: MainFrameHandle | undefined): boolean {
  return !!senderFrame && !!mainFrame && senderFrame === mainFrame;
}

/** The refusal every `operations:*` handler gives a sender that fails `isMainFrame`. Plain sentence, no detail an attacker could use. */
export const NOT_MAIN_FRAME = "operation controls are only accepted from the main window";

const OPERATION_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;

/** Shared with `operations:decision`'s own `stepId` check, so `operations:input` validates it the same way even though the bench's `/operations/:id/input` route never forwards it. */
export function operationStepId(value: unknown): string {
  if (typeof value !== "string" || !OPERATION_ID.test(value)) throw new Error("not a step id");
  return value;
}
