import { For, Show, createSignal } from "solid-js";
import type { AuthState } from "../../auth/controller";
import { Button } from "../ui/Button";
import { screen, type LoginAction } from "../login";

/** The whole window until the person is signed in and connected. Holds no credential. */
export function LoginScreen(props: { state: AuthState }) {
  const v = () => screen(props.state);
  const has = (a: LoginAction) => v().actions.includes(a);
  const [editing, setEditing] = createSignal(false);
  const [address, setAddress] = createSignal("");
  const [note, setNote] = createSignal("");
  const openAddress = async () => {
    setAddress(await window.harness.auth.api());
    setNote("");
    setEditing(true);
  };
  const saveAddress = () =>
    window.harness.auth.setApi(address()).then(
      () => setEditing(false),
      (e: Error) => setNote(e.message),
    );

  return (
    <div class="flex h-screen flex-col items-center justify-center gap-4 bg-bg px-4 text-fg" style={{ "-webkit-app-region": "drag" }}>
      <div class="flex w-full max-w-[360px] flex-col items-center gap-3 text-center" style={{ "-webkit-app-region": "no-drag" }}>
        <h1 class="text-base font-medium">{v().title}</h1>
        <Show when={v().code}>{(c) => <div class="select-text font-mono text-2xl tracking-widest">{c()}</div>}</Show>
        <Show when={v().body}>{(b) => <p class="text-sm text-subtle">{b()}</p>}</Show>
        <Show when={v().url}>{(u) => <p class="select-text break-all font-mono text-xs text-subtle">{u()}</p>}</Show>
        <Show when={v().teams}>
          {(ts) => (
            <div class="flex w-full flex-col gap-1.5">
              <For each={ts()}>
                {(t) => (
                  <Button disabled={t.disabled} onClick={() => void window.harness.auth.chooseTeam(t.slug)}>
                    {t.label}
                    <Show when={t.note}>{(n) => <span class="ml-2 text-xs text-subtle">{n()}</span>}</Show>
                  </Button>
                )}
              </For>
            </div>
          )}
        </Show>
        <div class="flex gap-2">
          <Show when={has("signIn")}><Button variant="primary" onClick={() => void window.harness.auth.signIn()}>Sign in with browser</Button></Show>
          <Show when={has("openBrowser")}><Button onClick={() => void window.harness.auth.openBrowser()}>Open browser again</Button></Show>
          <Show when={has("cancel")}><Button onClick={() => void window.harness.auth.cancel()}>Cancel</Button></Show>
          <Show when={has("retry")}><Button variant="primary" onClick={() => void window.harness.auth.retry()}>Retry</Button></Show>
          <Show when={has("signOut")}><Button onClick={() => void window.harness.auth.signOut()}>Sign out</Button></Show>
        </div>
        <Show when={has("address")}>
          <Show when={editing()} fallback={<button class="text-xs text-subtle underline" onClick={() => void openAddress()}>Kloudlite address</button>}>
            <div class="flex w-full gap-2">
              <input
                class="h-6 flex-1 rounded-[2px] border border-input-line bg-input px-1.5 font-mono text-sm outline-none focus:border-focus"
                placeholder="https://dev.kloudlite.io"
                value={address()}
                onInput={(e) => setAddress(e.currentTarget.value)}
              />
              <Button size="sm" onClick={() => void saveAddress()}>Save</Button>
            </div>
            <Show when={note()}>{(n) => <p class="text-xs text-danger">{n()}</p>}</Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}
