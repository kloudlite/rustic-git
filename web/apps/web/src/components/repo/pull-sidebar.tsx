/** The PR's right-hand rail.
 *
 *  Four of these five sections have no backend yet. They are drawn as the design
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

function Nothing({ children }: { children: React.ReactNode }) {
  return <p className="text-caption text-muted-foreground">{children}</p>;
}

export function PullSidebar() {
  return (
    <aside className="grid content-start divide-y divide-border border-border text-sm2">
      <div className="pb-5"><Section title="Reviewers"><Nothing>No reviews</Nothing></Section></div>
      <div className="py-5"><Section title="Assignees"><Nothing>No one assigned</Nothing></Section></div>
      <div className="py-5"><Section title="Labels"><Nothing>None yet</Nothing></Section></div>
      <div className="py-5"><Section title="Milestone"><Nothing>No milestone</Nothing></Section></div>
      <div className="py-5">
        <Section title="Development">
          <Nothing>No linked issues</Nothing>
        </Section>
      </div>
      <div className="pt-5"><Section title="Checks"><Nothing>No checks configured</Nothing></Section></div>
    </aside>
  );
}
