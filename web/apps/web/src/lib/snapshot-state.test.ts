import { describe, expect, test } from "bun:test";
import { stateSummary } from "./snapshot-state";

describe("stateSummary", () => {
  test("a workspace names its image and package count", () => {
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq", "rg", "fd", "bat"], quotaGb: 5, resources: {} })).toBe("alpine:3.20 · 4 packages");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq"], quotaGb: 5, resources: {} })).toBe("alpine:3.20 · 1 package");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: [], quotaGb: 5, resources: {} })).toBe("alpine:3.20");
  });
  test("an environment counts its services", () => {
    expect(stateSummary({ kind: "environment", services: [{ name: "db", image: "x" }, { name: "api", image: "y" }, { name: "web", image: "z" }], quotaGb: 5 })).toBe("3 services");
    expect(stateSummary({ kind: "environment", services: [{ name: "db", image: "x" }], quotaGb: 5 })).toBe("1 service");
  });
  test("no state renders nothing", () => {
    expect(stateSummary(null)).toBe("");
    expect(stateSummary(undefined)).toBe("");
  });
});
