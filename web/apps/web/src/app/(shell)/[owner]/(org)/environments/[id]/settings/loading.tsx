import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** env-settings.tsx: General, Danger zone. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={2} subtitle={false} />
    </Skeleton>
  );
}
