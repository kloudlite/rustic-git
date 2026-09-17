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
/**
 * `wired` is "a person can pick a model here TODAY". Only DeepSeek has credentials on the fleet
 * (spec §1.1); the rest are listed so the picker shows the whole shape of what pi supports and
 * dims what is not configured, rather than pretending the world is one provider.
 */
export const PROVIDERS: { id: string; label: string; wired?: boolean }[] = [
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "deepseek", label: "DeepSeek", wired: true },
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

/**
 * The rest of pi's providers: their whole credential is env values or an OAuth flow, not a key, so
 * pi's own `/login` configures them and `setProvider` must never accept one — they are deliberately
 * NOT in `PROVIDERS`, which is the key-writing trust boundary. The `/model` picker lists them so
 * the person sees the whole shape of what pi supports, dimmed and unpickable.
 */
export const EXTERNAL_PROVIDERS: { id: string; label: string }[] = [
  { id: "amazon-bedrock", label: "Amazon Bedrock" },
  { id: "azure-openai-responses", label: "Azure OpenAI" },
  { id: "google-vertex", label: "Google Vertex AI" },
  { id: "anthropic-oauth", label: "Anthropic (Claude Pro/Max)" },
  { id: "github-copilot", label: "GitHub Copilot" },
  { id: "openai-codex", label: "OpenAI Codex" },
];

/** Every provider the picker lists, wired or not. Only DeepSeek is wired on the fleet (spec §1.1). */
export const allProviders = (): { id: string; label: string; wired: boolean }[] => [...PROVIDERS, ...EXTERNAL_PROVIDERS].map((p) => ({ id: p.id, label: p.label, wired: ("wired" in p && p.wired) === true }));

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
  return PROVIDERS.map((p) => ({ ...p, wired: p.wired === true, configured: data[p.id]?.type === "api_key" && typeof data[p.id]?.key === "string" && data[p.id].key !== "" }));
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
