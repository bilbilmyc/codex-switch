import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { StatusIcon } from "./StatusIcon";

describe("status bar icons", () => {
  it.each([
    ["success", "circle-check"],
    ["warning", "circle-alert"],
    ["error", "circle-x"],
    [undefined, "info"],
  ] as const)("uses a distinct vector icon for %s", (tone, name) => {
    const markup = renderToStaticMarkup(<StatusIcon busy={false} tone={tone} />);
    expect(markup).toContain(`lucide-${name}`);
    expect(markup).toContain('width="18"');
    expect(markup).toContain('height="18"');
    expect(markup).not.toContain("legacy-status-dot");
  });

  it("shows a spinner while working", () => {
    const markup = renderToStaticMarkup(<StatusIcon busy tone="success" />);
    expect(markup).toContain("lucide-refresh-cw");
    expect(markup).toContain(" spin");
  });
});
