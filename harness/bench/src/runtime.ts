// The real engine wiring (Task 10). Until it lands, the bench still starts read-only.
import type { Turn } from "./session.ts";

export function makeTurn(): Turn {
  return async () => {
    throw new Error("engine not wired");
  };
}
