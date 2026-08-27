import { expect, test } from "bun:test";
import { hrefFor } from "./view-as";

test("the public view is a query on the same page", () => {
  expect(hrefFor("acme", "member")).toBe("/acme");
  expect(hrefFor("acme", "public")).toBe("/acme?view=public");
});
