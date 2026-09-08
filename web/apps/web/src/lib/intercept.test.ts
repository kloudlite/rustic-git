import { describe, expect, test } from "bun:test";
import { interceptSummary } from "./intercept";
import type { ApiService } from "./api";

const svc = (name: string, ports: number[], intercepted_by?: string | null): ApiService => ({
  name, image: "img", command: [], env: {}, mounts: [], ports,
  ...(intercepted_by === undefined ? {} : { intercepted_by }),
});

describe("interceptSummary", () => {
  test("a service nobody wishes to intercept is held by nobody", () => {
    expect(interceptSummary(svc("api", [8080]), [])).toEqual({ heldBy: null, ports: [] });
    // A wish for a DIFFERENT service is not this one's.
    expect(interceptSummary(svc("api", [8080]), [{ service: "web", workspace: "ws-1", ports: [] }]))
      .toEqual({ heldBy: null, ports: [] });
    // The wish list is absent on an environment written before intercepts existed.
    expect(interceptSummary(svc("api", [8080]), undefined)).toEqual({ heldBy: null, ports: [] });
  });

  test("a wish reports its workspace and the mapping", () => {
    expect(
      interceptSummary(svc("api", [8080]), [{ service: "api", workspace: "ws-1", ports: [{ service: 8080, workspace: 3000 }] }]),
    ).toEqual({ heldBy: "ws-1", ports: [{ service: 8080, workspace: 3000 }] });
  });

  test("a declared port the wish does not map is answered on the same number", () => {
    expect(
      interceptSummary(svc("api", [8080, 9090]), [{ service: "api", workspace: "ws-1", ports: [{ service: 9090, workspace: 9091 }] }]),
    ).toEqual({ heldBy: "ws-1", ports: [{ service: 8080, workspace: 8080 }, { service: 9090, workspace: 9091 }] });
  });

  test("the wish is not what is in force: heldBy reads the wish alone", () => {
    // `intercepted_by` null with a wish present is the state the page must render on its own —
    // the helper never conflates the two, so heldBy still names the wish.
    expect(interceptSummary(svc("api", [8080], null), [{ service: "api", workspace: "ws-1", ports: [] }]).heldBy)
      .toBe("ws-1");
  });
});
