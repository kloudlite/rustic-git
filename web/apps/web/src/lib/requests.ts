import { DIMS, type QuotaDim } from "@/lib/quota";

/** The four kinds, in the order the picker offers them: most common first. The words are the
 *  api's own `spec.kind` values, so a doc's kind is directly a key here — one vocabulary. */
export const KINDS = ["quota", "access", "region", "other"] as const;
export type RequestKind = (typeof KINDS)[number];

export function kindLabel(k: RequestKind): string {
  return { quota: "More quota", access: "Team access", region: "A region", other: "Something else" }[k];
}

export type RequestDoc = {
  id: string;
  owner: string;
  kind: RequestKind;
  requestedBy: string;
  reason: string;
  quota?: Partial<Record<QuotaDim, number>>;
  access?: { team: string; role: string };
  region?: { region: string };
  other?: { title: string; body: string };
  state: "pending" | "approved" | "denied";
  decidedBy?: string | null;
  decidedAt?: string | null;
  note?: string | null;
  resolution?: string | null;
  createdAt?: string | null;
};

/** Exactly one block, matching the kind — the api 422s anything else, so the form decides here
 *  rather than discovering it after a round trip. Throws a sentence fit to show. */
export function blockFor(kind: RequestKind, form: FormData): Record<string, unknown> {
  const str = (k: string) => String(form.get(k) ?? "").trim();
  if (kind === "quota") {
    const quota: Partial<Record<QuotaDim, number>> = {};
    for (const d of DIMS) {
      const raw = str(d);
      if (!raw) continue;
      const n = Number(raw);
      if (!Number.isFinite(n) || n < 0) throw new Error(`That is not a valid amount for ${d}.`);
      quota[d] = n;
    }
    if (Object.keys(quota).length === 0) throw new Error("Raise at least one dimension.");
    return { quota };
  }
  if (kind === "access") {
    const team = str("team");
    const role = str("role");
    if (!team) throw new Error("Name the team.");
    if (!["member", "admin", "owner"].includes(role)) throw new Error("Pick a role.");
    return { access: { team, role } };
  }
  if (kind === "region") {
    const region = str("region");
    if (!region) throw new Error("Pick a region.");
    return { region: { region } };
  }
  const title = str("title");
  const body = str("body");
  if (!title || !body) throw new Error("A title and a description, please.");
  return { other: { title, body } };
}

/** One line summarizing whichever block is set — the list row shows this instead of dumping the
 *  raw block, since only one of the four is ever present on a given doc. */
export function summarize(doc: RequestDoc): string {
  if (doc.quota) {
    return Object.entries(doc.quota)
      .map(([d, n]) => `${d}: ${n}`)
      .join(", ");
  }
  if (doc.access) return `${doc.access.team} as ${doc.access.role}`;
  if (doc.region) return doc.region.region;
  if (doc.other) return doc.other.title;
  return "";
}
