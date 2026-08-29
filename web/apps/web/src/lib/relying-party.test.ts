import { expect, test } from "bun:test";
import { relyingPartyFor } from "./relying-party";

test("the relying party comes from AUTH_URL, not the request", () => {
  expect(relyingPartyFor("https://app.example.com", "app.example.com")).toEqual({
    rpID: "app.example.com",
    origin: "https://app.example.com",
    rpName: "kloudlite",
  });
  expect(relyingPartyFor("https://app.example.com", undefined).rpID).toBe("app.example.com");
});

test("a forged X-Forwarded-Host is refused rather than honoured", () => {
  expect(() => relyingPartyFor("https://app.example.com", "evil.example.com")).toThrow();
  expect(() => relyingPartyFor("https://app.example.com", "evil.example.com, app.example.com")).toThrow();
});

test("without AUTH_URL the request host stands in, localhost as http", () => {
  expect(relyingPartyFor(undefined, "localhost:3000")).toEqual({
    rpID: "localhost",
    origin: "http://localhost:3000",
    rpName: "kloudlite",
  });
  expect(relyingPartyFor(undefined, "tunnel.example.dev").origin).toBe("https://tunnel.example.dev");
});
