/** Shown while a repo page's api calls are in flight. Blocks the shape of the
 *  code view — toolbar, crumb, listing — so the page does not jump when it lands. */
export default function Loading() {
  return (
    <div className="animate-pulse" aria-busy="true" aria-label="Loading">
      <div className="flex items-center gap-3">
        <div className="h-8 w-36 bg-muted" />
        <div className="h-8 w-64 bg-muted" />
        <div className="ml-auto h-8 w-24 bg-muted" />
      </div>
      <div className="mt-5 h-5 w-48 bg-muted" />
      <div className="mt-3 border border-border bg-card">
        {Array.from({ length: 8 }, (_, i) => (
          <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-3 last:border-b-0">
            <div className="size-4 bg-muted" />
            <div className="h-4 w-40 bg-muted" />
            <div className="ml-auto h-3 w-16 bg-muted" />
          </div>
        ))}
      </div>
    </div>
  );
}
