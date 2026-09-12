import { describe, expect, test } from "bun:test";
import { renderReadme } from "./readme";

/** The three things react-markdown did for free and this renderer now has to do itself. */
describe("renderReadme", () => {
  test("raw HTML in the source is dropped, not rendered", async () => {
    const html = await renderReadme('<script>alert(1)</script>\n\nhello <b onclick="x()">there</b>\n');
    expect(html).not.toContain("<script");
    expect(html).not.toContain("onclick");
    expect(html).toContain("hello");
  });

  test("only a protocol a document may name survives", async () => {
    const html = await renderReadme("[click](javascript:alert(1)) [ok](https://example.com) [rel](./a.md)\n");
    expect(html).not.toContain("javascript:");
    expect(html).toContain('href="https://example.com"');
    expect(html).toContain('href="./a.md"');
  });

  test("a quote in a title or an alt cannot close the attribute", async () => {
    const html = await renderReadme('![a" onerror="x](https://e.com/i.png)\n\n[t](https://e.com "a\\" onmouseover=\\"x")\n');
    // Escaped, so the attribute closes where the renderer put the quote and not where the
    // author put one: `&quot;` is text in the alt, never the end of the value.
    expect(html).not.toContain('onerror="');
    expect(html).not.toContain('onmouseover="');
    expect(html).toContain("&quot; onerror=&quot;x");
  });

  test("gfm tables and fences still render", async () => {
    const html = await renderReadme("| a | b |\n| - | - |\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n");
    expect(html).toContain("<table");
    expect(html).toContain("shiki");
  });

  test("a mermaid fence keeps its source for the client to draw", async () => {
    const html = await renderReadme("```mermaid\ngraph TD; A-->B;\n```\n");
    expect(html).toContain("data-mermaid");
    expect(html).toContain("graph TD; A--&gt;B;");
  });
});
