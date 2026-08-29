import { describe, expect, mock, test } from "bun:test";

mock.module("server-only", () => ({}));
const { provenanceOf } = await import("./env-page");

describe("provenanceOf", () => {
  test("an object is read as-is, anything else is empty", () => {
    expect(provenanceOf({ name: "web", services: [] })).toEqual({ name: "web", services: [] });
    expect(provenanceOf(null)).toEqual({});
    expect(provenanceOf("x")).toEqual({});
    expect(provenanceOf(undefined)).toEqual({});
  });
});
