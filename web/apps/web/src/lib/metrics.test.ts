import { beforeEach, describe, expect, test } from "bun:test";
import { count, observe, render, reset, routeLabel, scrapeAllowed } from "./metrics";

beforeEach(reset);

describe("routeLabel", () => {
  test("a section keeps its name, a handle does not", () => {
    expect(routeLabel("/superadmin/quotas")).toBe("/superadmin");
    expect(routeLabel("/api/health")).toBe("/api");
    expect(routeLabel("/")).toBe("/");
    // The whole point: a per-repo path must not mint a per-repo time series.
    expect(routeLabel("/alice/rustic-git/tree/main")).toBe("/:owner");
    expect(routeLabel("/bob/other")).toBe("/:owner");
  });
});

describe("render", () => {
  test("counters group under one HELP/TYPE and labels sort", () => {
    count("http_requests_total", { route: "/api", method: "GET", status: "200" });
    count("http_requests_total", { route: "/api", method: "GET", status: "200" });
    count("auth_failures_total", { reason: "bad_password" });
    const out = render();
    expect(out).toContain('http_requests_total{method="GET",route="/api",status="200"} 2');
    expect(out).toContain('auth_failures_total{reason="bad_password"} 1');
    expect(out.match(/# TYPE http_requests_total counter/g)).toHaveLength(1);
    // Never emitted at all rather than emitted empty: an unused metric is not a zero.
    expect(out).not.toContain("upstream_requests_total");
  });

  test("a quote in a label cannot break the scrape", () => {
    count("upstream_requests_total", { upstream: 'a"b', status: "500" });
    expect(render()).toContain('upstream_requests_total{status="500",upstream="a\\"b"} 1');
  });

  test("the histogram is cumulative and always carries +Inf", () => {
    observe(0.02);
    observe(3);
    observe(-1); // not a duration; dropped rather than counted
    const out = render();
    expect(out).toContain('http_request_duration_seconds_bucket{le="0.025"} 1');
    expect(out).toContain('http_request_duration_seconds_bucket{le="1"} 1');
    expect(out).toContain('http_request_duration_seconds_bucket{le="5"} 2');
    expect(out).toContain('http_request_duration_seconds_bucket{le="+Inf"} 2');
    expect(out).toContain("http_request_duration_seconds_count 2");
  });
});

describe("scrapeAllowed", () => {
  const h = (o: Record<string, string>) => new Headers(o);

  test("the pod's own address and port is the scrape", () => {
    expect(scrapeAllowed(h({ host: "10.42.0.7:3000" }))).toBe(true);
    // Next sets these on every request, including a direct one, so they cannot gate anything.
    expect(scrapeAllowed(h({ host: "10.42.0.7:3000", "x-forwarded-for": "1.2.3.4" }))).toBe(true);
  });

  test("anything through the ingress is refused", () => {
    expect(scrapeAllowed(h({ host: "dev.kloudlite.io" }))).toBe(false);
    expect(scrapeAllowed(h({ host: "dev.kloudlite.io:443" }))).toBe(false);
    expect(scrapeAllowed(h({}))).toBe(false);
  });

  test("the public host is refused even on the container port", () => {
    const was = process.env.AUTH_URL;
    process.env.AUTH_URL = "https://dev.kloudlite.io";
    expect(scrapeAllowed(h({ host: "dev.kloudlite.io:3000" }))).toBe(false);
    expect(scrapeAllowed(h({ host: "10.42.0.7:3000" }))).toBe(true);
    process.env.AUTH_URL = was;
  });
});
