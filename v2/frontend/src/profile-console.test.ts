import { describe, expect, it } from "vitest";
import {
  describeApplyState,
  filterProfiles,
  profileSummaryToDraft,
  quickModelDraftState,
} from "./profile-console";
import type { ProfileApplyState } from "./types";
import type { ProfileSummary } from "./types";

const profiles: ProfileSummary[] = [
  {
    id: "relay-a",
    name: "Team Relay",
    baseUrl: "https://relay.example.com/v1",
    model: "gpt-5.2-codex",
    reviewModel: "gpt-5.2",
    hasApiKey: true,
    isActive: true,
    applyState: "applied",
  },
  {
    id: "relay-b",
    name: "Z.ai Backup",
    baseUrl: "https://open.bigmodel.cn/api/paas/v4",
    model: "GLM-5.3",
    hasApiKey: true,
    isActive: false,
    applyState: "inactive",
  },
];

describe("profile console", () => {
  it("filters profiles by name, model, or base URL without case sensitivity", () => {
    expect(filterProfiles(profiles, "team")).toEqual([profiles[0]]);
    expect(filterProfiles(profiles, "glm-5.3")).toEqual([profiles[1]]);
    expect(filterProfiles(profiles, "BIGMODEL.CN")).toEqual([profiles[1]]);
  });

  it("trims the query and preserves profile order for broad matches", () => {
    expect(filterProfiles(profiles, "  HTTPS://  ")).toEqual(profiles);
    expect(filterProfiles(profiles, "   ")).toBe(profiles);
    expect(filterProfiles(profiles, "missing")).toEqual([]);
  });

  it("creates a draft that preserves the stored API key", () => {
    expect(profileSummaryToDraft(profiles[0])).toEqual({
      name: "Team Relay",
      baseUrl: "https://relay.example.com/v1",
      apiKey: undefined,
      clearApiKey: false,
      model: "gpt-5.2-codex",
      reviewModel: "gpt-5.2",
    });
  });

  it("keeps an absent review model absent", () => {
    expect(profileSummaryToDraft(profiles[1]).reviewModel).toBeUndefined();
  });

  it.each<[ProfileApplyState, string, string, string]>([
    ["applied", "当前 Codex 路由", "生效", "success"],
    ["pending_changes", "当前中转站有未应用修改", "待应用", "warning"],
    ["external_drift", "Codex 配置已被外部修改", "外部变更", "error"],
    ["unknown", "无法确认当前 Codex 路由", "待确认", "unknown"],
    ["inactive", "已选择，尚未应用", "", "staged"],
  ])("describes the %s apply state without conflating it with another state", (state, kicker, badge, tone) => {
    expect(describeApplyState(state)).toMatchObject({ kicker, badge, tone });
  });

  it("tracks a quick model draft independently from whether it can be saved", () => {
    expect(quickModelDraftState("gpt-5.2-codex", "gpt-5.6-sol")).toEqual({
      normalized: "gpt-5.6-sol",
      dirty: true,
      valid: true,
    });
    expect(quickModelDraftState("gpt-5.2-codex", "   ")).toEqual({
      normalized: "",
      dirty: true,
      valid: false,
    });
    expect(quickModelDraftState("gpt-5.2-codex", " gpt-5.2-codex ").dirty).toBe(false);
  });
});
