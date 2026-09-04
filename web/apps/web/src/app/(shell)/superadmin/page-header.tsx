/** One line of title plus purpose, shared by every tab in the operations area so a person
 *  jumping between them always lands the same way. */
export function PageHeader({ title, purpose }: { title: string; purpose: string }) {
  return (
    <div className="mb-6">
      <h1 className="text-base font-medium">{title}</h1>
      <p className="text-sm2 text-muted-foreground">{purpose}</p>
    </div>
  );
}
