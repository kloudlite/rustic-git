import { Button } from "@/components/ui/button";

function GitHubIcon() {
  return (
    <svg viewBox="0 0 24 24" className="size-[18px]" fill="currentColor" aria-hidden>
      <path d="M12 .5C5.73.5.9 5.48.9 11.92c0 5.05 3.22 9.33 7.69 10.84.56.11.77-.25.77-.56v-2.1c-3.13.7-3.79-1.38-3.79-1.38-.51-1.34-1.25-1.7-1.25-1.7-1.02-.72.08-.7.08-.7 1.13.08 1.72 1.19 1.72 1.19 1 1.77 2.63 1.26 3.28.96.1-.75.39-1.26.71-1.55-2.5-.29-5.13-1.29-5.13-5.72 0-1.27.44-2.3 1.16-3.11-.12-.29-.5-1.47.11-3.05 0 0 .95-.31 3.1 1.19a10.5 10.5 0 0 1 5.65 0c2.15-1.5 3.1-1.19 3.1-1.19.61 1.58.23 2.76.11 3.05.72.81 1.16 1.84 1.16 3.11 0 4.44-2.64 5.42-5.15 5.71.41.36.77 1.07.77 2.15v3.19c0 .31.2.68.78.56 4.46-1.52 7.68-5.79 7.68-10.84C23.1 5.48 18.27.5 12 .5Z" />
    </svg>
  );
}

function GoogleIcon() {
  return (
    <svg viewBox="0 0 24 24" className="size-[18px]" aria-hidden>
      <path fill="#4285F4" d="M23.5 12.27c0-.79-.07-1.54-.2-2.27H12v4.51h6.47a5.53 5.53 0 0 1-2.4 3.58v3h3.86c2.26-2.09 3.57-5.17 3.57-8.82Z" />
      <path fill="#34A853" d="M12 24c3.24 0 5.96-1.08 7.93-2.91l-3.86-3c-1.08.72-2.45 1.16-4.07 1.16-3.13 0-5.78-2.11-6.73-4.96H1.29v3.09A12 12 0 0 0 12 24Z" />
      <path fill="#FBBC05" d="M5.27 14.29a7.2 7.2 0 0 1 0-4.58v-3.1H1.29a12 12 0 0 0 0 10.78l3.98-3.1Z" />
      <path fill="#EA4335" d="M12 4.75c1.77 0 3.35.61 4.6 1.8l3.42-3.42C17.95 1.19 15.24 0 12 0A12 12 0 0 0 1.29 6.61l3.98 3.1C6.22 6.86 8.87 4.75 12 4.75Z" />
    </svg>
  );
}

function MicrosoftIcon() {
  return (
    <svg viewBox="0 0 24 24" className="size-[18px]" aria-hidden>
      <path fill="#F25022" d="M2 2h9.4v9.4H2z" />
      <path fill="#7FBA00" d="M12.6 2H22v9.4h-9.4z" />
      <path fill="#00A4EF" d="M2 12.6h9.4V22H2z" />
      <path fill="#FFB900" d="M12.6 12.6H22V22h-9.4z" />
    </svg>
  );
}

const PROVIDERS = [
  { name: "GitHub", Icon: GitHubIcon },
  { name: "Google", Icon: GoogleIcon },
  { name: "Microsoft", Icon: MicrosoftIcon },
] as const;

/** Icons sit at a fixed inset, labels start at one shared x. Centring icon+label
 *  as a unit — the obvious way — gives three labels at three different offsets,
 *  which is what makes a stack of provider buttons look ragged. */
export function OAuthButtons({ verb }: { verb: "Sign in" | "Sign up" }) {
  return (
    <div className="grid gap-2">
      {PROVIDERS.map(({ name, Icon }) => (
        <Button
          key={name}
          variant="outline"
          className="h-11 w-full justify-start gap-3 px-4 text-[14px] font-semibold"
        >
          <span className="flex w-[18px] shrink-0 justify-center">
            <Icon />
          </span>
          <span>
            {verb} with {name}
          </span>
        </Button>
      ))}
    </div>
  );
}

export function OrDivider() {
  return (
    <div className="flex items-center gap-3" role="separator">
      <span className="h-px flex-1 bg-border" />
      <span className="text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
        or
      </span>
      <span className="h-px flex-1 bg-border" />
    </div>
  );
}
