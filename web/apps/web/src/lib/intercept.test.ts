import { describe, expect, test } from "bun:test";
import { interceptSummary } from "./intercept";
import type { ApiEnvironment, ApiService } from "./api";

const svc = (name: string, ports: number[]): ApiService => ({
  name, image: "img", command: [], env: {}, mounts: [], ports,
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
    // Nothing in force (an empty `service_status`, or an entry with a null `intercepted_by`) while
    // a wish is present is the state the page must render on its own — the helper is given only
    // the wish and never reaches for status, so heldBy still names the wish.
    const env: Pick<ApiEnvironment, "service_status"> = {
      service_status: [{ name: "api", ready: true, intercepted_by: null }],
    };
    expect(env.service_status?.[0].intercepted_by ?? null).toBeNull();
    expect(interceptSummary(svc("api", [8080]), [{ service: "api", workspace: "ws-1", ports: [] }]).heldBy)
      .toBe("ws-1");
  });
});
