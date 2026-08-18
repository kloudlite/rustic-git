import Image from "next/image";
import { cn } from "@/lib/utils";

/** Official assets from the kloudlite brand kit. The colour logo carries dark text and
 *  the white logo carries light text, so we ship both and swap on theme rather than
 *  recolouring — the kit forbids altering logo colours. */
export function Logo({ className }: { className?: string }) {
  return (
    <>
      <Image
        src="/brand/kloudlite-logo-color.svg"
        alt="kloudlite"
        width={628}
        height={131}
        priority
        className={cn("h-6 w-auto dark:hidden", className)}
      />
      <Image
        src="/brand/kloudlite-logo-white.svg"
        alt="kloudlite"
        width={628}
        height={131}
        priority
        className={cn("hidden h-6 w-auto dark:block", className)}
      />
    </>
  );
}

/** Icon only, for tight spaces. Minimum 24px per the brand kit. */
export function Icon({ className }: { className?: string }) {
  return (
    <>
      <Image src="/brand/kloudlite-icon-primary.svg" alt="" width={130} height={131}
             className={cn("size-6 dark:hidden", className)} />
      <Image src="/brand/kloudlite-icon-white.svg" alt="" width={130} height={131}
             className={cn("hidden size-6 dark:block", className)} />
    </>
  );
}
