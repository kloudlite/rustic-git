"use client";

import Link from "next/link";
import { Menu } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { SidebarNav } from "@/components/app/sidebar-nav";
import { UserMenu } from "@/components/app/user-menu";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetTitle, SheetTrigger } from "@/components/ui/sheet";
import type { Session } from "@/lib/session";

/** Below lg the sidebar becomes a top bar and a drawer. The drawer holds the
 *  same SidebarNav, so the two never disagree about what the sections are. */
export function MobileNav({ session, active }: { session: NonNullable<Session>; active?: string }) {
  return (
    <header className="sticky top-0 z-40 flex h-14 items-center gap-2 border-b border-border bg-background px-4 lg:hidden">
      <Sheet>
        <SheetTrigger asChild>
          <Button variant="outline" size="icon" aria-label="Open navigation" className="border-edge">
            <Menu />
          </Button>
        </SheetTrigger>
        <SheetContent side="left" className="w-72 p-0">
          <SheetTitle className="sr-only">Navigation</SheetTitle>
          <div className="flex h-14 items-center px-4">
            <Logo className="h-5" />
          </div>
          <div className="px-3">
            <SidebarNav active={active} />
          </div>
        </SheetContent>
      </Sheet>
      <Link href="/" aria-label="kloudlite home" className="inline-flex">
        <Logo className="h-5" />
      </Link>
      <div className="flex-1" />
      <UserMenu name={session.user.name} email={session.user.email} />
    </header>
  );
}
