import { useMemo } from "react";
import { MacOSScrollAccel } from "@opentui/core";

// opentui's wheel default is LinearScrollAccel — one row per tick, which makes
// a long transcript or file tree feel stuck. This is its macOS curve instead: a
// slow gesture still steps one row at a time, a fast burst ramps up.
//
// The accelerator holds a velocity window between ticks, so it has to survive
// re-renders (a per-render `new` resets the streak every repaint) but must NOT
// be shared: the SSH server runs every client in one process, and one window
// fed by two people wheeling at once inflates both their multipliers.
export const useWheelAccel = () => useMemo(() => new MacOSScrollAccel(), []);
