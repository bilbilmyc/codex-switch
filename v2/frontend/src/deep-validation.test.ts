import { describe, expect, it } from "vitest";
import {
  deepValidationActionLabel,
  deepValidationPresentation,
  formatDeepValidationTime,
} from "./deep-validation";

describe("deep validation presentation", () => {
  it("formats the backend check time as absolute local time", () => {
    const localTime = new Date(2026, 8, 4, 23, 7, 9).getTime();

    expect(formatDeepValidationTime(localTime)).toBe("2026-09-04 23:07:09");
  });

  it("maps safe failure categories without exposing response content", () => {
    const presentation = deepValidationPresentation({
      state: "result",
      result: {
        status: "failed",
        requestDurationMs: 812.6,
        checkedAtUnixMs: new Date(2026, 8, 4, 22, 0, 0).getTime(),
        errorCategory: "response_too_large",
        usage: { inputTokens: 12, outputTokens: 3, totalTokens: 15 },
      },
    });

    expect(presentation).toMatchObject({
      tone: "error",
      title: "验证失败",
      category: "响应超过安全上限",
      duration: "813 ms",
      usage: "共 15 tokens · 输入 12 · 输出 3",
    });
    expect(JSON.stringify(presentation)).not.toContain("output");
    expect(deepValidationActionLabel({ state: "result", result: { status: "failed", requestDurationMs: 1, checkedAtUnixMs: 1, errorCategory: "request_rejected" } })).toBe("重试");
  });
});
