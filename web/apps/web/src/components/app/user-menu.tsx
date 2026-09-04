"use client";

import Link from "next/link";
import { Inbox, LogOut, Settings, ShieldCheck } from "lucide-react";
import { Initials } from "@/components/app/initials";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { signOutAction } from "@/app/(auth)/actions";

/** Identity, the way to your settings, and sign-out live behind the avatar. Theme
 *  is a preference, and preferences have a page — it lives in Settings, not here. */
export function UserMenu({
  name,
  email,
  superadmin,
}: {
  name: string;
  email: string;
  superadmin: boolean;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label="Account"
        className="outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <Initials name={name} size={8} tone="primary" />
      </DropdownMenuTrigger>

      <DropdownMenuContent align="end" className="w-60">
        <DropdownMenuLabel className="font-normal">
          <div className="text-sm2 font-medium text-foreground">{name}</div>
          <div className="truncate text-caption text-muted-foreground">{email}</div>
        </DropdownMenuLabel>

        <DropdownMenuSeparator />

        <DropdownMenuItem asChild>
          <Link href="/settings"><Settings className="size-4" /> Profile settings</Link>
        </DropdownMenuItem>

        <DropdownMenuItem asChild>
          <Link href="/requests"><Inbox className="size-4" /> My requests</Link>
        </DropdownMenuItem>

        {superadmin ? (
          <DropdownMenuItem asChild>
            <Link href="/superadmin"><ShieldCheck className="size-4" /> Superadmin</Link>
          </DropdownMenuItem>
        ) : null}

        <DropdownMenuSeparator />

        <DropdownMenuItem
         
          onSelect={() => {
            void signOutAction();
          }}
        >
          <LogOut className="size-4" /> Sign out
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
