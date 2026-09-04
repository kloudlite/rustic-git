import { NextResponse } from "next/server";
import { apiToken } from "@/lib/api-token";
import { getSession } from "@/lib/session";
import { adminAuditCsv } from "@/lib/api";
import type { AuditFilter } from "@/lib/audit";

/** Proxies `GET /admin/audit.csv` — a plain link to the admin service would need the bearer
 *  token in the URL for the browser to hold; this route holds it instead and streams the CSV
 *  back with the download header, same as any other file the app hands the browser. */
export async function GET(req: Request) {
  const session = await getSession();
  if (!session?.user.superadmin) return NextResponse.json({ error: "not found" }, { status: 404 });
  const token = await apiToken();
  if (!token) return NextResponse.json({ error: "unauthorized" }, { status: 401 });

  const params = new URL(req.url).searchParams;
  const filter: AuditFilter = {};
  for (const key of ["actor", "action", "target", "from", "to"] as const) {
    const v = params.get(key);
    if (v) filter[key] = v;
  }

  const res = await adminAuditCsv(token, filter);
  const body = await res.text();
  if (!res.ok) return NextResponse.json({ error: body || "the service is unavailable" }, { status: res.status || 502 });

  return new Response(body, {
    headers: {
      "content-type": "text/csv",
      "content-disposition": 'attachment; filename="audit.csv"',
    },
  });
}
