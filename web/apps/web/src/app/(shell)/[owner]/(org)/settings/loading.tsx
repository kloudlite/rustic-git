import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** team-settings.tsx: title, then Team / Public profile / Visibility / Members / Danger zone.
 *  The last three render only for admins and owners; the skeleton draws the admin shape,
 *  because a settings page a plain member opens is the rarer of the two. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={5} />
    </Skeleton>
  );
}
