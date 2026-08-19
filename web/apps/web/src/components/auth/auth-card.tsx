import { cn } from "@/lib/utils";

/** The contained surface every auth screen sits in. On light, the page ground is a
 *  step darker than the card so the card reads as a surface, not a border drawn
 *  on white; on dark the card token is already a step lighter than the page. */
export function AuthCard({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <div className={cn("border border-border bg-card p-8 text-card-foreground", className)}>
      {children}
    </div>
  );
}

/** One heading block for every auth screen. Centred: the column is centred on the
 *  page and the logo is centred above it, so a left-aligned title would be the one
 *  thing off-axis. Fields below stay left-aligned — forms read left to right. */
export function AuthHeader({
  title,
  children,
}: {
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <div className="mb-6 text-center">
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

/** The line under the card: "New here? Create an account". Outside the card on
 *  purpose — it is a way out of this screen, not part of the task on it. */
export function AuthFootnote({ children }: { children: React.ReactNode }) {
  return <p className="mt-6 text-center text-sm2 text-muted-foreground">{children}</p>;
}
