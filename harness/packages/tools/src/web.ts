import type { ToolDef } from "./index.ts";

/** Guard every outbound call: a hung host must not hang the turn. */
async function get(url: string, headers: Record<string, string> = {}, body?: unknown): Promise<Response> {
  const res = await fetch(url, {
    ...(body === undefined ? {} : { method: "POST", body: JSON.stringify(body) }),
    headers: { "user-agent": "kloudlite-tui", ...headers },
    redirect: "follow",
    signal: AbortSignal.timeout(20_000),
  });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return res;
}

/**
 * HTML to something a model can read. Deliberately a few regexes rather than a
 * parser dependency: script/style go first (their contents are not text), then
 * tags, then entities. A real parser only matters if we start needing structure.
 */
function toText(html: string): string {
  return html
    .replace(/<(script|style|noscript|svg)\b[^>]*>[\s\S]*?<\/\1>/gi, " ")
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/<\/(p|div|h[1-6]|li|tr|section|article|br)\s*>/gi, "\n")
    .replace(/<[^>]+>/g, " ")
    .replace(/&nbsp;/gi, " ")
    .replace(/&amp;/gi, "&")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&quot;/gi, '"')
    .replace(/&#39;|&apos;/gi, "'")
    .replace(/&#(\d+);/g, (_, n) => String.fromCharCode(Number(n)))
    .replace(/[ \t]+/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** Keep a page from eating the context window; the model can fetch again. */
function clip(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max)}\n\n… truncated at ${max} characters`;
}

export const webFetch = {
  name: "web_fetch",
  description:
    "Fetch a URL and return its readable text. Use for documentation, changelogs, issues, or any page whose contents you need to read. JSON comes back verbatim; HTML is stripped to text. Not a search engine — you need the exact URL.",
  inputSchema: {
    type: "object",
    properties: {
      url: { type: "string", description: "Absolute http(s) URL." },
      maxChars: {
        type: "number",
        description: "Truncate the result at this many characters. Default 50000.",
      },
    },
    required: ["url"],
  },
  async run({ url, maxChars = 50_000 }) {
    if (!/^https?:\/\//i.test(url)) return `error: ${url} is not an http(s) URL`;
    try {
      const res = await get(url);
      const type = res.headers.get("content-type") ?? "";
      const body = await res.text();
      // JSON and plain text are already readable; only markup needs stripping
      const text = /json|text\/plain|xml/i.test(type) ? body : toText(body);
      return clip(text, maxChars) || "(the page had no readable text)";
    } catch (e) {
      return `error fetching ${url}: ${(e as Error).message}`;
    }
  },
} satisfies ToolDef<{ url: string; maxChars?: number }>;

/**
 * Search providers, in the order we try them. Each is a key in auth.json or the
 * environment; the first one configured wins. Adding another is one entry here.
 */
const SEARCH: Record<
  string,
  { env: string; url: (q: string, n: number) => string; headers: (k: string) => Record<string, string>; parse: (j: any) => { title: string; url: string; text: string }[] }
> = {
  brave: {
    env: "BRAVE_API_KEY",
    url: (q, n) => `https://api.search.brave.com/res/v1/web/search?q=${encodeURIComponent(q)}&count=${n}`,
    headers: (k) => ({ "x-subscription-token": k, accept: "application/json" }),
    parse: (j) =>
      (j?.web?.results ?? []).map((r: any) => ({
        title: r.title ?? "",
        url: r.url ?? "",
        text: r.description ?? "",
      })),
  },
  tavily: {
    env: "TAVILY_API_KEY",
    url: () => "https://api.tavily.com/search",
    headers: (k) => ({ authorization: `Bearer ${k}`, "content-type": "application/json" }),
    parse: (j) =>
      (j?.results ?? []).map((r: any) => ({
        title: r.title ?? "",
        url: r.url ?? "",
        text: r.content ?? "",
      })),
  },
};

/** The configured provider, or undefined when no key is set anywhere. */
export function searchProvider(): { name: string; key: string } | undefined {
  for (const [name, p] of Object.entries(SEARCH)) {
    const key = process.env[p.env];
    if (key) return { name, key };
  }
  return undefined;
}

export const webSearch = {
  name: "web_search",
  description:
    "Search the web and return titles, URLs and snippets. Use when you need to find a page rather than read one you already have the URL for; follow up with web_fetch to read a result.",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string", description: "What to search for." },
      count: { type: "number", description: "How many results. Default 5, max 10." },
    },
    required: ["query"],
  },
  async run({ query, count = 5 }) {
    const provider = searchProvider();
    // fail loud and actionable: a silent empty result reads like "nothing found"
    if (!provider)
      return `error: web search needs an API key. Set one of ${Object.values(SEARCH)
        .map((p) => p.env)
        .join(" or ")} and restart.`;
    const p = SEARCH[provider.name]!;
    const n = Math.min(Math.max(count, 1), 10);
    try {
      const body = provider.name === "tavily" ? { query, max_results: n } : undefined;
      const res = await get(p.url(query, n), p.headers(provider.key), body);
      const hits = p.parse(await res.json()).slice(0, n);
      if (hits.length === 0) return `No results for "${query}".`;
      return hits.map((h) => `${h.title}\n${h.url}\n${h.text}`).join("\n\n");
    } catch (e) {
      return `error searching for "${query}": ${(e as Error).message}`;
    }
  },
} satisfies ToolDef<{ query: string; count?: number }>;
