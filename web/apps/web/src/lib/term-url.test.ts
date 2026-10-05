import { describe, expect, test } from "bun:test";
import { termUrl } from "./term-url";

describe("termUrl", () => {
  test("drops the /tunnel/{id} path, keeping only the gateway's origin", () => {
    expect(termUrl("wss://ws-eu.khost.dev/tunnel/b-1", "b-1", "t")).toBe("https://ws-eu.khost.dev/term/b-1/?token=t");
  });

  test("maps ws: to http: for a dev gateway", () => {
    expect(termUrl("ws://localhost:7789/tunnel/b-1", "b-1", "t")).toBe("http://localhost:7789/term/b-1/?token=t");
  });

  test("the brief's bare-host case still works", () => {
    expect(termUrl("wss://gw.example", "bench-1", "t")).toBe("https://gw.example/term/bench-1/?token=t");
  });
});
