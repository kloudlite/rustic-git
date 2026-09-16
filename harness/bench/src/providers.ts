import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * Model provider keys, as pi stores them. pi reads credentials from
 * `$HOME/.pi/agent/auth.json` (`{ "<id>": { "type": "api_key", "key": "..." } }`,
 * 0600); this is the same file, written from the desktop Settings instead of
 * pi's own `/login`. A key only ever goes IN — `list` answers whether one is
 * configured, never the value, and nothing here is logged.
 *
 * The list is the API-key providers of pi's `docs/providers.md` table whose
 * whole credential IS a key; the ones needing extra env (Bedrock, Azure,
 * Cloudflare, Vertex) and the OAuth-only ones are left to pi's own `/login`.
 */
export const PROVIDERS: { id: string; label: string }[] = [
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "deepseek", label: "DeepSeek" },
  { id: "google", label: "Google Gemini" },
  { id: "xai", label: "xAI" },
  { id: "openrouter", label: "OpenRouter" },
  { id: "groq", label: "Groq" },
  { id: "cerebras", label: "Cerebras" },
  { id: "mistral", label: "Mistral" },
  { id: "nvidia", label: "NVIDIA NIM" },
  { id: "together", label: "Together AI" },
  { id: "fireworks", label: "Fireworks" },
  { id: "baseten", label: "Baseten" },
  { id: "huggingface", label: "Hugging Face" },
  { id: "zai", label: "ZAI Coding Plan" },
  { id: "kimi-coding", label: "Kimi For Coding" },
  { id: "minimax", label: "MiniMax" },
  { id: "opencode", label: "OpenCode Zen" },
];

type Entry = { type?: string; key?: string } & Record<string, unknown>;

/** pi's own resolution: the env override first, else `~/.pi/agent`. */
export const authPath = () => path.join(process.env.PI_CODING_AGENT_DIR ?? path.join(process.env.HOME ?? os.homedir(), ".pi", "agent"), "auth.json");

function load(file: string): Record<string, Entry> {
  let raw: string;
  try {
    raw = fs.readFileSync(file, "utf-8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return {};
    throw e;
  }
  const v = JSON.parse(raw.replace(/^﻿/, "")) as unknown;
  // Refuse rather than clobber: an unreadable auth.json holds someone's OAuth logins too.
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error("auth.json is not an object");
  return v as Record<string, Entry>;
}

/** Same dir, then rename: a torn write would leave pi with no credentials at all. */
function save(file: string, data: Record<string, Entry>) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.tmp-${process.pid}`;
  fs.writeFileSync(tmp, JSON.stringify(data, null, 2), { encoding: "utf-8", mode: 0o600 });
  fs.renameSync(tmp, file);
}

export function listProviders(file = authPath()) {
  const data = load(file);
  return PROVIDERS.map((p) => ({ ...p, configured: data[p.id]?.type === "api_key" && typeof data[p.id]?.key === "string" && data[p.id].key !== "" }));
}

export function setProvider(id: string, apiKey: unknown, file = authPath()) {
  if (!PROVIDERS.some((p) => p.id === id)) throw new Error(`unknown provider ${JSON.stringify(id)}`);
  if (typeof apiKey !== "string" || apiKey.trim() === "") throw new Error("apiKey required");
  const data = load(file);
  // Merge, so a provider's other fields (env values pi supports) survive a key change.
  save(file, { ...data, [id]: { ...data[id], type: "api_key", key: apiKey.trim() } });
}

export function removeProvider(id: string, file = authPath()) {
  if (!PROVIDERS.some((p) => p.id === id)) throw new Error(`unknown provider ${JSON.stringify(id)}`);
  const data = load(file);
  if (!(id in data)) return;
  delete data[id];
  save(file, data);
}
