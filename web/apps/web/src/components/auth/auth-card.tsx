/** One heading block for every auth screen, so the type scale and the rhythm
 *  above the first control never drift between pages or between steps. */
export function AuthHeader({
  title,
  children,
}: {
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <div className="mb-6">
      <h1 className="text-title font-semibold leading-title tracking-title">{title}</h1>
      {children ? (
        <p className="mt-1.5 text-sm2 leading-relaxed text-muted-foreground">{children}</p>
      ) : null}
    </div>
  );
}

/** Label row above an input. `aside` is the optional right-hand link ("Forgot?").
 *  Both sit on the same baseline so the row reads as one line, not two elements. */
export function FieldLabel({
  htmlFor,
  children,
  aside,
}: {
  htmlFor: string;
  children: React.ReactNode;
  aside?: React.ReactNode;
}) {
  return (
    <div className="flex items-baseline justify-between gap-4">
      <label htmlFor={htmlFor} className="text-sm2 font-medium leading-none">
        {children}
      </label>
      {aside}
    </div>
  );
}
