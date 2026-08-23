/** A page for something that does not exist yet, saying so. Honest and blank beats
 *  a mock-up that invites clicks which go nowhere. */
export function NotYet({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section>
      <h1 className="text-title font-semibold tracking-title">{title}</h1>
      <p className="mt-6 border border-border bg-card px-5 py-14 text-center text-sm2 text-muted-foreground">
        {children}
      </p>
    </section>
  );
}
