"use client";

import Link from "next/link";
import { LogOut, Settings } from "lucide-react";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
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
export function UserMenu({ name, email }: { name: string; email: string }) {
  const initials = name.split(" ").map((p) => p[0]).slice(0, 2).join("");

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label="Account"
        className="outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <Avatar className="size-8">
          <AvatarFallback className="bg-primary text-micro font-semibold text-primary-foreground">
            {initials}
          </AvatarFallback>
        </Avatar>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="end" className="w-60">
        <DropdownMenuLabel className="font-normal">
          <div className="text-sm2 font-medium text-foreground">{name}</div>
          <div className="truncate text-caption text-muted-foreground">{email}</div>
        </DropdownMenuLabel>

        <DropdownMenuSeparator />

        <DropdownMenuItem asChild>
          <Link href="/settings"><Settings className="size-4" /> Settings</Link>
        </DropdownMenuItem>

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
