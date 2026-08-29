import { describe, expect, mock, test } from "bun:test";

mock.module("server-only", () => ({}));
const { fenceLang, langFor } = await import("./highlight");

describe("langFor", () => {
  test("by extension, then by whole name, else text", () => {
    expect(langFor("src/main.rs")).toBe("rust");
    expect(langFor("deploy/Dockerfile")).toBe("dockerfile");
    expect(langFor("a/.gitignore")).toBe("ini");
    expect(langFor("notes.UNKNOWN")).toBe("text");
    expect(langFor(".bashrc")).toBe("text");
  });
});

describe("fenceLang", () => {
  test("accepts a grammar name or an extension and nothing else", () => {
    expect(fenceLang("rs")).toBe("rust");
    expect(fenceLang("Rust")).toBe("rust");
    expect(fenceLang("console")).toBe("text");
    expect(fenceLang(undefined)).toBe("text");
  });
});
