import { render } from "solid-js/web";
import "./fonts.css";
import "./styles/app.css";
import { App } from "./App";
import type { Harness } from "../preload";

declare global {
  interface Window { harness: Harness }
}

render(() => <App />, document.getElementById("root")!);
