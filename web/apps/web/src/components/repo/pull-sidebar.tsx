/** The PR's right-hand rail.
 *
 *  All five of these sections have no backend yet. They are drawn as the design
 *  has them, saying plainly that there is nothing rather than inventing a
 *  reviewer or a green check — a page that claims a review happened is worse than
 *  one that admits none has. Each becomes real when its backend lands, and the
 *  layout does not move when it does. */
function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">{title}</h2>
      <div className="mt-2">{children}</div>
    </div>
  );
}

/** An empty section is a dash, not a sentence: six sentences saying nothing is
 *  noise, and the heading above already says what is missing. */
function Nothing() {
  return <p className="text-sm2 text-muted-foreground">None</p>;
}

export function PullSidebar() {
  return (
    <aside className="grid content-start gap-4 divide-y divide-border border-border text-sm2">
      <div className="pb-4"><Section title="Reviewers"><Nothing /></Section></div>
      <div className="py-4"><Section title="Assignees"><Nothing /></Section></div>
      <div className="py-4"><Section title="Labels"><Nothing /></Section></div>
      <div className="py-4"><Section title="Development"><Nothing /></Section></div>
      <div className="pt-4"><Section title="Checks"><Nothing /></Section></div>
    </aside>
  );
}
