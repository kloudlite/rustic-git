import { SettingsBones, Skeleton } from "@/components/app/skeleton";

/** user-settings.tsx: title, then seven `SettingsSection`s. It draws its own container. */
export default function Loading() {
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <Skeleton>
        <SettingsBones sections={5} />
      </Skeleton>
    </main>
  );
}
