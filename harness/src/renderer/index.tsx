import { render } from "solid-js/web";
import "./fonts.css";
import "./styles/app.css";
import { App } from "./App";
import type { Harness } from "../preload";

declare global {
  interface Window { harness: Harness }
}

// Paint once, with the faces: the first frame after a reload used to be the
// fallback font and the intro, then everything swapped. Load the two faces
// the UI is set in before anything renders; a face that fails to load does
// not hold the app hostage (the fallback stack is there for that).
void Promise.all([document.fonts.load('13px "IBM Plex Sans"'), document.fonts.load("13px Lilex")])
  .catch(() => undefined)
  .then(() => render(() => <App />, document.getElementById("root")!));
