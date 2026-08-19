/** A settings section: heading and rationale on the left, controls on the right.
 *  Every section on a settings page shares the shape, so the left column reads
 *  as a table of contents. */
export function SettingsSection({
  title,
  description,
  children,
  danger,
}: {
  title: string;
  description: string;
  children: React.ReactNode;
  danger?: boolean;
}) {
  return (
    <section className="grid gap-6 border-t border-border py-10 first:border-t-0 first:pt-0 md:grid-cols-settings md:gap-12">
      <div>
        <h2 className={`text-body font-semibold ${danger ? "text-destructive" : ""}`}>{title}</h2>
        <p className="mt-1.5 text-sm2 leading-relaxed text-muted-foreground">{description}</p>
      </div>
      <div className="min-w-0">{children}</div>
    </section>
  );
}
