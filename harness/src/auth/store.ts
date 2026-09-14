import fs from "node:fs";
import type { Credential } from "./device";

/**
 * The desktop login on disk: one file in userData, encrypted by the OS keychain through
 * Electron's safeStorage (injected, so this runs under node --test). No keychain means no
 * login is kept at all — a plaintext fallback would be a bearer token on disk.
 */
export type Crypto = { isEncryptionAvailable(): boolean; encryptString(s: string): Buffer; decryptString(b: Buffer): string };

export class NoKeychain extends Error {
  constructor() {
    super("this computer has no keychain Kloudlite can use, so your login cannot be stored safely");
    this.name = "NoKeychain";
  }
}

export function createStore(file: string, crypto: Crypto) {
  const need = () => {
    if (!crypto.isEncryptionAvailable()) throw new NoKeychain();
  };
  return {
    load(): Credential | undefined {
      need();
      try {
        const c = JSON.parse(crypto.decryptString(fs.readFileSync(file))) as Credential;
        return typeof c.token === "string" && typeof c.api === "string" ? c : undefined;
      } catch {
        return undefined; // missing, or written by another keychain: signed out, not an error
      }
    },
    save(c: Credential): void {
      need();
      const tmp = `${file}.tmp`;
      fs.writeFileSync(tmp, crypto.encryptString(JSON.stringify(c)), { mode: 0o600 });
      fs.renameSync(tmp, file);
    },
    clear(): void {
      fs.rmSync(file, { force: true });
    },
  };
}
