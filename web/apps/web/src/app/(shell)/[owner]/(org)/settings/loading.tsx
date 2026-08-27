import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** team-settings.tsx: title, then Team / Public profile / Visibility / Members / Danger zone.
 *  Public profile, Visibility and Danger zone are role-gated; the skeleton draws the OWNER
 *  shape (all five) — an admin sees four, a plain member two, and both are the rarer open. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={5} />
    </Skeleton>
  );
}
