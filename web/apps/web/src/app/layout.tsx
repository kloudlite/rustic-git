import type { Metadata } from "next";
import localFont from "next/font/local";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import "./globals.css";

// GitHub's type: Mona Sans for text, Hubot Sans for headings, Mona Sans Mono for code. All
// three are variable fonts under the SIL OFL (licenses beside the files), self-hosted so a
// page never waits on a third-party font host. The width axis is declared through
// `font-stretch`, which is what makes the condensed/expanded cuts one file rather than three.
// Hubot Sans ships its variable cut as TTF only (~710 KB); this is that file subset to Latin
// (Google Fonts' `latin` range) and recompressed as woff2 (~200 KB) with all three axes kept:
//   pyftsubset HubotSansVF.ttf --unicodes="U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,
//     U+02DA,U+02DC,U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,
//     U+2215,U+FEFF,U+FFFD" --layout-features='*' --flavor=woff2
// It is heading-only and loads with `swap`, so text never waits on it either way.
const sans = localFont({
  src: "./fonts/MonaSansVF.woff2",
  variable: "--font-sans-brand",
  weight: "200 900",
  declarations: [{ prop: "font-stretch", value: "75% 125%" }],
  display: "swap",
});

const heading = localFont({
  src: "./fonts/HubotSansVF.woff2",
  variable: "--font-heading-brand",
  weight: "200 900",
  declarations: [{ prop: "font-stretch", value: "75% 125%" }],
  display: "swap",
});

const mono = localFont({
  src: "./fonts/MonaSansMonoVF.woff2",
  variable: "--font-mono-brand",
  weight: "200 900",
  display: "swap",
});

export const metadata: Metadata = {
  title: { default: "kloudlite", template: "%s · kloudlite" },
  description: "Code repos, package registries, workspaces, environments and CI triggers — one system, for you and your agents.",
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    // suppressHydrationWarning: the theme script sets `class` on <html> before React
    // hydrates, which is the whole point — it prevents a flash of the wrong theme.
    <html lang="en" className={`${sans.variable} ${heading.variable} ${mono.variable}`} suppressHydrationWarning>
      <body className="antialiased">
        <ThemeProvider>
          <TooltipProvider delayDuration={300}>{children}</TooltipProvider>
        </ThemeProvider>
      </body>
    </html>
  );
}
