import { expect, test } from "bun:test";
import { TurnWords, consented, target } from "./consent.ts";

test("case and whitespace are normalised", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "please delete workspace foo" }, ["Please  DELETE workspace foo now"])).toBe(true);
});

test("a quote that does not name the target fails", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "delete it now please" }, ["delete it now please"])).toBe(false);
});

test("a quote the person did not type fails", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "delete workspace foo" }, ["stop workspace bar"])).toBe(false);
});

test("a short quote fails", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "foo" }, ["foo"])).toBe(false);
});

test("a reason alone never consents", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { reason: "cleanup" }, ["delete workspace foo"])).toBe(false);
});

test("a workspace session's own workspace is the default target", () => {
  expect(consented("workspace_stop", {}, { asked: "stop this workspace demo" }, ["stop this workspace demo"], "demo")).toBe(true);
});

test("exec binds the program", () => {
  const q = { asked: "run cargo test please" };
  expect(consented("exec", { cmd: "cargo test -p x" }, q, ["run cargo test please"])).toBe(true);
  expect(consented("exec", { cmd: "curl https://x.io" }, q, ["run cargo test please"])).toBe(false);
  expect(consented("exec", { cmd: ["/usr/bin/cargo", "build"] }, { asked: "run cargo build" }, ["run cargo build"])).toBe(true);
  expect(target("exec", { cmd: "FOO=1 cargo test" })).toBe("cargo");
});

test("bash reads command", () => {
  expect(target("bash", { command: "ls -la" })).toBe("ls");
});

test("web_fetch binds the host", () => {
  const q = { asked: "read docs.rs for tokio" };
  expect(consented("web_fetch", { url: "https://docs.rs/tokio" }, q, ["read docs.rs for tokio"])).toBe(true);
  expect(consented("web_fetch", { url: "https://evil.com/docs.rs" }, q, ["read docs.rs for tokio"])).toBe(false);
  expect(target("web_fetch", { url: "not a url" })).toBeUndefined();
});

test("image targets drop registry path, tag and digest", () => {
  expect(target("container_build", { tags: ["team/hello:1"] })).toBe("hello");
  expect(target("container_push", { src: "hello@sha256:ab" })).toBe("hello");
});

test("an unknown tool has no target and never consents", () => {
  expect(target("read", { path: "x" })).toBeUndefined();
  expect(consented("read", { path: "x" }, { asked: "read the file x" }, ["read the file x"])).toBe(false);
});

test("TurnWords keeps what is typed mid-turn", () => {
  const w = new TurnWords();
  w.add("a");
  w.start();
  w.add("b");
  w.end();
  expect(w.get()).toEqual(["b"]);
  w.start();
  w.end();
  expect(w.get()).toEqual([]);
});

test("packages_remove needs every package named", () => {
  const a = { packages: ["jq", "ripgrep@14"] };
  expect(consented("packages_remove", a, { asked: "remove jq and ripgrep" }, ["remove jq and ripgrep"])).toBe(true);
  expect(consented("packages_remove", a, { asked: "remove jq please" }, ["remove jq please"])).toBe(false);
});

test("service_update and intercept name their service", () => {
  expect(consented("service_update", { service: { name: "web", image: "x" } }, { asked: "update the web service" }, ["update the web service"])).toBe(true);
  expect(consented("intercept", { service: "api" }, { asked: "intercept api into here" }, ["intercept api into here"])).toBe(true);
});

test("a quote cut mid-name fails, a sentence-closing period does not", () => {
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "delete workspace foo" }, ["delete workspace foo-old"])).toBe(false);
  expect(consented("workspace_delete", { workspace: "foo" }, { asked: "delete workspace foo" }, ["delete workspace foo."])).toBe(true);
  expect(consented("web_fetch", { url: "https://example.com/x" }, { asked: "fetch example.com" }, ["fetch example.com.evil.io"])).toBe(false);
});

test("a quote counts from lent words and not from the relayed request text", () => {
  const w = new TurnWords();
  const call = { asked: "delete workspace foo" };
  expect(consented("workspace_delete", { workspace: "foo" }, call, w.get())).toBe(false); // only main's request said it
  w.add("please delete workspace foo");
  expect(consented("workspace_delete", { workspace: "foo" }, call, w.get())).toBe(true);
});
