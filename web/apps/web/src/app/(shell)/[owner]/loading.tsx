/** Shown while any owner-scoped page's api calls are in flight — the repo list,
 *  the registries, the workspaces. They are all a heading plus a listing, so one
 *  skeleton covers the segment and the page does not jump when it lands. */
export default function Loading() {
  return (
    <div className="animate-pulse" aria-busy="true" aria-label="Loading">
      <div className="flex items-center gap-3">
        <div className="size-10 bg-muted" />
        <div className="h-8 w-40 bg-muted" />
        <div className="ml-auto h-8 w-28 bg-muted" />
      </div>
      <div className="mt-6 h-9 w-64 bg-muted" />
      <div className="mt-4 border border-border bg-card">
        {Array.from({ length: 6 }, (_, i) => (
          <div key={i} className="border-b border-border px-4 py-4 last:border-b-0">
            <div className="h-4 w-48 bg-muted" />
            <div className="mt-2 h-3 w-72 bg-muted" />
          </div>
        ))}
      </div>
    </div>
  );
}
