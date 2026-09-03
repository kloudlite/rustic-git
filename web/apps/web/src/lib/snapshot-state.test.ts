import { describe, expect, test } from "bun:test";
import { stateSummary, type SnapshotState } from "./snapshot-state";

const resources = {
  cpuRequest: "2",
  cpuLimit: "4",
  memoryRequest: "4Gi",
  memoryLimit: "8Gi",
};

describe("stateSummary", () => {
  test("a workspace names its image and package count", () => {
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq", "rg", "fd", "bat"], quotaGb: 5, resources })).toBe("alpine:3.20 · 4 packages");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq"], quotaGb: 5, resources })).toBe("alpine:3.20 · 1 package");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: [], quotaGb: 5, resources })).toBe("alpine:3.20");
  });
  test("an environment counts its services", () => {
    const svc = (name: string) => ({ name, image: "x", command: [], env: {}, mounts: [], ports: [] });
    expect(stateSummary({ kind: "environment", services: [svc("db"), svc("api"), svc("web")], quotaGb: 5 })).toBe("3 services");
    expect(stateSummary({ kind: "environment", services: [svc("db")], quotaGb: 5 })).toBe("1 service");
  });
  test("no state renders nothing", () => {
    expect(stateSummary(null)).toBe("");
    expect(stateSummary(undefined)).toBe("");
  });
  // The row the api actually sends, `crd::PodResources` and all: this fails to COMPILE if the
  // type drifts from what `/v1/volumes/{name}/history` serializes.
  test("a state parsed from an api row type-checks", () => {
    // A literal, not `JSON.parse` — parse returns `any` and would type-check against anything.
    const row = {
      kind: "workspace",
      image: "alpine:3.20",
      packages: ["jq", "rg"],
      quotaGb: 10,
      attachedEnvironment: null,
      resources: { cpuRequest: "2", cpuLimit: "4", memoryRequest: "4Gi", memoryLimit: "8Gi" },
    } satisfies SnapshotState;
    expect(row.kind === "workspace" && row.resources.memoryLimit).toBe("8Gi");
    expect(stateSummary(row)).toBe("alpine:3.20 · 2 packages");

    // The environment half of the same guard: the api freezes the WHOLE `model::Service`, so this
    // fails to compile if `ApiService` and what `/v1/volumes/{name}/history` sends drift apart.
    const envRow = {
      kind: "environment",
      quotaGb: 20,
      services: [
        {
          name: "db",
          image: "busybox:1.36",
          command: ["sh", "-c", "httpd -f"],
          env: { MODE: "test" },
          mounts: [{ folder: "data", path: "/ws" }],
          ports: [27017],
        },
      ],
    } satisfies SnapshotState;
    expect(stateSummary(envRow)).toBe("1 service");
  });
});
