import { NextResponse } from "next/server";
import { apiToken } from "@/lib/api-token";
import { listRepos } from "@/lib/api";

/** The ⌘K palette's data, fetched when it OPENS. This used to ride in every
 *  page's RSC payload — every repo of every owner, on every hard load. */
export async function GET(req: Request) {
  const owner = new URL(req.url).searchParams.get("owner");
  if (!owner) return NextResponse.json({ error: "owner is required" }, { status: 400 });
  const token = await apiToken();
  if (!token) return NextResponse.json([], { status: 401 });
  const list = await listRepos(token, owner);
  if (!list.ok) return NextResponse.json([], { status: list.kind === "notFound" ? 404 : 502 });
  // Only what the palette draws — never the whole ApiRepo.
  return NextResponse.json(
    list.value.map((r) => ({ owner: r.owner, name: r.name, public: r.public, description: r.description })),
  );
}
