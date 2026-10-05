import { describe, expect, test } from "bun:test";
import { termUrl } from "./term-url";

describe("termUrl", () => {
  test("turns the gateway's wss address into the https term page", () => {
    expect(termUrl("wss://gw.example", "bench-1", "t")).toBe("https://gw.example/term/bench-1/?token=t");
  });
});
