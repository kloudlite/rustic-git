import { Show, createSignal } from "solid-js";
import { render } from "solid-js/web";
import "./fonts.css";
import "./styles/app.css";
import { App } from "./App";
import { LoginScreen } from "./components/LoginScreen";
import type { Harness } from "./harness.ts";
import type { AuthState } from "../auth/controller";

declare global {
  interface Window { harness: Harness }
}

/** Nothing of the app — not the cached session list, not a slash command — mounts before the person is signed in and connected. */
function Gate() {
  const [state, setState] = createSignal<AuthState>({ phase: "starting" });
  let pushed = false;
  const bootTest = new URLSearchParams(location.search).has("boot-test");
  window.harness.auth.onState((s) => ((pushed = true), setState(s)));
  // A pushed state is newer than the reply to a status asked before it, and may carry a sign-out reason the controller does not keep.
  void window.harness.auth.status().then((s) => pushed || setState(s));
  return (
    <Show when={bootTest || state().phase === "ready"} fallback={<LoginScreen state={state()} />}>
      <App />
    </Show>
  );
}

// Paint once, with the faces: the first frame after a reload used to be the
// fallback font and the intro, then everything swapped. Load the two faces
// the UI is set in before anything renders; a face that fails to load does
// not hold the app hostage (the fallback stack is there for that).
void Promise.all([document.fonts.load('13px "IBM Plex Sans"'), document.fonts.load('13px "IBM Plex Mono"')])
  .catch(() => undefined)
  .then(() => render(() => <Gate />, document.getElementById("root")!));
