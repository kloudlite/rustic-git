import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** team-settings.tsx: title, then Team / Members / Danger zone sections. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={3} />
    </Skeleton>
  );
}
