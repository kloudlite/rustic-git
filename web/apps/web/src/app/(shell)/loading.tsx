/** Shown while the home page's api calls are in flight. Blocks the shape of the
 *  signed-in home — feed on the left, repo rail on the right — so the page does
 *  not jump when it lands. */
export default function Loading() {
  return (
    <div className="animate-pulse" aria-busy="true" aria-label="Loading">
      <div className="h-8 w-48 bg-muted" />
      <div className="mt-6 grid gap-8 lg:grid-cols-[1fr_18rem]">
        <div className="border border-border bg-card">
          {Array.from({ length: 8 }, (_, i) => (
            <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-3 last:border-b-0">
              <div className="size-4 bg-muted" />
              <div className="h-4 w-56 bg-muted" />
              <div className="ml-auto h-3 w-16 bg-muted" />
            </div>
          ))}
        </div>
        <div className="border border-border bg-card">
          {Array.from({ length: 5 }, (_, i) => (
            <div key={i} className="border-b border-border px-4 py-3 last:border-b-0">
              <div className="h-4 w-32 bg-muted" />
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
