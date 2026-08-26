import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** repo-settings.tsx: General, Visibility, Protected branches, Danger zone. */
export default function Loading() {
  return (
    <Skeleton>
      <SettingsBones sections={4} />
    </Skeleton>
  );
}
