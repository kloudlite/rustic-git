import { For, Show, createResource, createSignal } from "solid-js";
import type { AuthState } from "../../auth/controller";
import { Button } from "../ui/Button";
import { Logo, Mark } from "../ui/Logo";
import { screen, type LoginAction } from "../login";

/** The row a team is chosen from: a button, so it is keyboard reachable, drawn as a list row. */
const ROW =
  "flex items-center justify-between h-10 px-3 border border-line rounded-[2px] text-sm " +
  "hover:bg-accent-wash focus-visible:outline focus-visible:outline-2 focus-visible:outline-focus " +
  "disabled:opacity-50 disabled:cursor-not-allowed disabled:hover:bg-transparent";

/** The whole window until the person is signed in and connected. Holds no credential. */
export function LoginScreen(props: { state: AuthState }) {
  const v = () => screen(props.state);
  const has = (a: LoginAction) => v().actions.includes(a);
  const [editing, setEditing] = createSignal(false);
  const [address, setAddress] = createSignal("");
  const [note, setNote] = createSignal("");
  // Resolved once for the footer: it only changes through Save below, which refetches.
  const [api, { refetch }] = createResource(() => window.harness.auth.api());
  const host = () => {
    try {
      return new URL(api() ?? "").hostname;
    } catch {
      return "dev.kloudlite.io";
    }
  };
  const openAddress = async () => {
    setAddress(await window.harness.auth.api());
    setNote("");
    setEditing(true);
  };
  const saveAddress = () =>
    window.harness.auth.setApi(address()).then(
      () => {
        setEditing(false);
        void refetch();
      },
      (e: Error) => setNote(e.message),
    );

  return (
    <div class="flex h-screen flex-col items-center justify-center gap-4 bg-bg px-4 text-fg" style={{ "-webkit-app-region": "drag" }}>
      <div class="w-[400px] max-w-full" style={{ "-webkit-app-region": "no-drag" }}>
        <div class="relative flex flex-col gap-5 rounded-[2px] border border-line bg-input px-10 py-9">
          <Show when={v().busy}>
            <div class="absolute inset-x-0 top-0 h-0.5 overflow-hidden bg-accent-wash">
              <div class="sweep h-full bg-accent" />
            </div>
          </Show>

          <Logo class="h-7" />
          <div class="flex flex-col gap-2">
            <h1 class={`text-xl font-semibold tracking-tight ${props.state.phase === "error" ? "text-danger" : "text-fg-strong"}`}>{v().title}</h1>
            <Show when={v().body}>{(b) => <p class="max-w-[60ch] text-sm leading-relaxed text-muted">{b()}</p>}</Show>
          </div>

          <Show when={v().code}>
            {(c) => (
              <div class="flex flex-col gap-2">
                <div class="select-text rounded-[2px] border border-input-line bg-bg px-4 py-3 text-center font-mono text-3xl tracking-[0.3em] text-fg-strong">{c()}</div>
                <Show when={v().url}>{(u) => <p class="select-text break-all font-mono text-xs text-subtle">{u()}</p>}</Show>
              </div>
            )}
          </Show>

          <Show when={v().teams}>
            {(ts) => (
              <div class="flex flex-col gap-1.5">
                <For each={ts()}>
                  {(t) => (
                    <button class={ROW} disabled={t.disabled} onClick={() => void window.harness.auth.chooseTeam(t.slug)}>
                      <span class="flex items-center gap-2">
                        <Mark class="size-4" />
                        {t.label}
                      </span>
                      <Show when={t.note}>{(n) => <span class="text-xs text-subtle">{n()}</span>}</Show>
                    </button>
                  )}
                </For>
              </div>
            )}
          </Show>

          <div class="flex flex-col gap-2 empty:hidden">
            <Show when={has("signIn")}>
              <Button variant="primary" size="lg" iconRight="external-link" class="w-full" onClick={() => void window.harness.auth.signIn()}>
                Continue in browser
              </Button>
            </Show>
            <Show when={has("openBrowser")}>
              <Button size="lg" class="w-full" onClick={() => void window.harness.auth.openBrowser()}>Open browser again</Button>
            </Show>
            <Show when={has("retry")}>
              <Button variant="primary" size="lg" class="w-full" onClick={() => void window.harness.auth.retry()}>Retry</Button>
            </Show>
            <Show when={has("cancel") || has("signOut")}>
              <div class="flex justify-end gap-2">
                <Show when={has("cancel")}><Button variant="ghost" size="sm" onClick={() => void window.harness.auth.cancel()}>Cancel</Button></Show>
                <Show when={has("signOut")}><Button variant="ghost" size="sm" onClick={() => void window.harness.auth.signOut()}>Sign out</Button></Show>
              </div>
            </Show>
          </div>
        </div>

        <Show when={has("address")}>
          <div class="mt-3">
            <Show
              when={editing()}
              fallback={
                <p class="text-center text-xs text-subtle">
                  Connected to <span class="font-mono">{host()}</span> ·{" "}
                  <button class="text-fg hover:underline" onClick={() => void openAddress()}>Change</button>
                </p>
              }
            >
              <div class="flex gap-2">
                <input
                  class="h-8 flex-1 rounded-[2px] border border-input-line bg-input px-1.5 font-mono text-sm outline-none focus:border-focus"
                  placeholder="https://dev.kloudlite.io"
                  value={address()}
                  onInput={(e) => setAddress(e.currentTarget.value)}
                />
                <Button onClick={() => void saveAddress()}>Save</Button>
                <Button variant="ghost" onClick={() => setEditing(false)}>Cancel</Button>
              </div>
              <Show when={note()}>{(n) => <p class="mt-1 text-xs text-danger">{n()}</p>}</Show>
            </Show>
          </div>
        </Show>
      </div>

      {/* No version: `window.harness.version` is Electron's, which means nothing to the person. */}
      <p class="fixed bottom-4 text-[11px] text-subtle">Kloudlite Desktop</p>
    </div>
  );
}
