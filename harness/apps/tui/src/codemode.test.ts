import { expect, test } from "bun:test";
import { createSession } from "@kloudlite-tui/agent";
import { catalog } from "./models.ts";

// Codemode needs two separate things to reach pi: the extension factory on a
// resource loader, and `codemode` activated by name. Naming `tools` at all
// replaces pi's default allowlist, so codemode is added on top of
// `getActiveToolNames()` instead — this asserts nothing is lost doing it. pi
// resolves the list when the session is built and never calls the model, so no
// credentials or network are involved.
const activeTools = async (codemode: boolean) => {
  const s = (await createSession({
    key: `codemode-test-${codemode}`,
    model: catalog[0] as never,
    codemode,
  })) as unknown as { getActiveToolNames: () => string[] };
  return s.getActiveToolNames();
};

test("codemode on adds it to pi's defaults instead of replacing them", async () => {
  const off = await activeTools(false);
  expect(off).not.toContain("codemode");
  // whatever pi defaults to, turning codemode on must only add to it
  expect(await activeTools(true)).toEqual([...off, "codemode"]);
});

// `mode: "only"` is what makes the model actually reach for it: pi hides the
// direct declarations and lists them in codemode's own description instead, so
// a script is the way to call them. The loadout is prepared per request, not at
// session build, so this asks the tool definition directly.
test("codemode hides the direct tools and lists them as callable", async () => {
  const s = (await createSession({
    key: "codemode-test-loadout",
    model: catalog[0] as never,
    codemode: true,
  })) as unknown as { _toolDefinitions: Map<string, { definition: any }> };
  const all = [...s._toolDefinitions.values()].map((v) => v.definition);
  const { hiddenDeclarations, descriptions } = all
    .find((d) => d.name === "codemode")
    .prepareLoadout({
      declared: all,
      callable: all,
      getExposure: (n: string) => (n === "codemode" ? "model-only" : "direct"),
      getNamespace: () => undefined,
      getPromptGuidelines: () => [],
    });
  expect(hiddenDeclarations).toContain("bash");
  expect(descriptions.codemode).toContain("grep");
});
