import type { Metadata } from "next";
import localFont from "next/font/local";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import "./globals.css";

// GitHub's type: Mona Sans for text, Hubot Sans for headings, Mona Sans Mono for code. All
// three are variable fonts under the SIL OFL (licenses beside the files), self-hosted so a
// page never waits on a third-party font host. The width axis is declared through
// `font-stretch`, which is what makes the condensed/expanded cuts one file rather than three.
// Hubot Sans ships its variable cut as TTF only (~700 KB); it is heading-only and loads with
// `swap`, so text never waits on it. ponytail: convert to woff2 if that size ever shows up.
const sans = localFont({
  src: "./fonts/MonaSansVF.woff2",
  variable: "--font-sans-brand",
  weight: "200 900",
  declarations: [{ prop: "font-stretch", value: "75% 125%" }],
  display: "swap",
});

const heading = localFont({
  src: "./fonts/HubotSansVF.ttf",
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
