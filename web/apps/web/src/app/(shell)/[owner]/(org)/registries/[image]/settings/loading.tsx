import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** image-settings.tsx: title with no subtitle, sections from y=194. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={2} subtitle={false} />
    </Skeleton>
  );
}
