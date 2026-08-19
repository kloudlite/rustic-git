import Link from "next/link";
import { Icon } from "@/components/brand/logo";
import { ScrollArea } from "@/components/ui/scroll-area";

/** The same centred column as sign-in: this is still the way in, and switching
 *  layouts between the last sign-in step and the first real one would read as
 *  landing somewhere else. */
export default function OnboardingLayout({ children }: { children: React.ReactNode }) {
  return (
    <ScrollArea className="h-screen">
      <div className="flex min-h-screen flex-col">
        <main className="flex flex-1 flex-col items-center justify-center px-6 py-16">
          <Link href="/" aria-label="kloudlite home" className="mb-8 inline-flex">
            <Icon className="size-9" />
          </Link>
          {children}
        </main>
      </div>
    </ScrollArea>
  );
}
