/** Loading-state primitives. A skeleton exists to hold the page's SHAPE so nothing jumps when
 *  the data lands — so every `loading.tsx` composes these using the same grid tokens
 *  (`grid-cols-overview`, `grid-cols-settings`, …) as the page it stands in for. A skeleton
 *  that draws a different layout is worse than a spinner: the page jumps twice. */

export function Bone({ className = "" }: { className?: string }) {
  return <div className={`bg-muted ${className}`} />;
}

/** Wraps a whole loading state: one pulse, one accessible label. */
export function Skeleton({ children, className = "" }: { children: React.ReactNode; className?: string }) {
  return (
    <div className={`animate-pulse ${className}`} aria-busy="true" aria-label="Loading">
      {children}
    </div>
  );
}

/** The search-and-tabs toolbar every filterable list opens with (`repo-list.tsx`'s shape). */
export function ToolbarBones() {
  return (
    <div className="flex flex-wrap items-center gap-3">
      <Bone className="h-8 w-full max-w-xs" />
      <Bone className="h-8 w-40" />
      <Bone className="ml-auto h-8 w-28" />
    </div>
  );
}

/** A bordered list of two-line rows: title, then a shorter meta line. */
export function ListBones({ rows = 6, className = "" }: { rows?: number; className?: string }) {
  return (
    <div className={`border border-border bg-card ${className}`}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="border-b border-border px-4 py-4 last:border-b-0">
          <Bone className="h-4 w-48" />
          <Bone className="mt-2 h-3 w-72 max-w-full" />
        </div>
      ))}
    </div>
  );
}

/** Single-line rows with a leading icon and trailing meta — file listings, feeds. */
export function LineBones({ rows = 8, className = "" }: { rows?: number; className?: string }) {
  return (
    <div className={`border border-border bg-card ${className}`}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-3 last:border-b-0">
          <Bone className="size-4 shrink-0" />
          <Bone className="h-4 w-56 max-w-[60%]" />
          <Bone className="ml-auto h-3 w-16" />
        </div>
      ))}
    </div>
  );
}

/** The settings page: a title, then `SettingsSection` rows — heading column, control column. */
export function SettingsBones({ sections = 3 }: { sections?: number }) {
  return (
    <>
      <Bone className="h-7 w-48" />
      <div className="mt-8">
        {Array.from({ length: sections }, (_, i) => (
          <div key={i} className="grid gap-6 border-t border-border py-10 first:border-t-0 first:pt-0 md:grid-cols-settings md:gap-12">
            <div>
              <Bone className="h-5 w-32" />
              <Bone className="mt-2 h-3 w-64 max-w-full" />
              <Bone className="mt-1.5 h-3 w-48" />
            </div>
            <div className="min-w-0">
              <Bone className="h-9 w-full max-w-md" />
              <Bone className="mt-3 h-9 w-full max-w-md" />
              <Bone className="mt-4 h-9 w-28" />
            </div>
          </div>
        ))}
      </div>
    </>
  );
}
