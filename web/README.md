# kloudlite web

Turborepo + bun. `apps/web` is the Next.js application.

    bun install
    bun run dev        # http://localhost:3000

## Design tokens

The brand is mapped onto shadcn's token system in `apps/web/src/app/globals.css`,
so no component hard-codes a colour and dark mode is a re-mapping rather than an
inversion.

| Brand | Token | Notes |
|---|---|---|
| `#2258E5` primary | `--primary`, `--ring`, `--chart-1..5` | lightened in dark so it holds contrast on `#09090B` |
| `#09090B` black | `--foreground` light / `--background` dark | |
| `#71717A` gray | `--muted-foreground` | |

`#09090B` and `#71717A` are exactly zinc-950 and zinc-500, so the neutral ramp is
the zinc scale and needed no invention.

**Sharp corners**: `--radius: 0rem`. shadcn derives every other radius from it, so a
single token change squares every component.

**Type**: Open Sans for UI, JetBrains Mono for identifiers and code.
