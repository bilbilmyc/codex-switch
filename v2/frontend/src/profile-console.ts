import type { ProfileApplyState, ProfileDraft, ProfileSummary } from "./types";

export type ApplyStatePresentation = {
  badge: string;
  kicker: string;
  action: string;
  routeLabel: string;
  routeDetail: string;
  readinessDetail: string;
  tone: "success" | "warning" | "error" | "staged" | "unknown";
  routeClass: "active" | "staged" | "drift" | "unknown";
  healthClass: "active" | "warning" | "error" | "idle";
  ready: boolean;
};

const applyStatePresentations: Record<ProfileApplyState, ApplyStatePresentation> = {
  inactive: {
    badge: "",
    kicker: "已选择，尚未应用",
    action: "应用到 Codex",
    routeLabel: "待应用",
    routeDetail: "当前只选中，尚未写入 Codex",
    readinessDetail: "尚未写入 Codex",
    tone: "staged",
    routeClass: "staged",
    healthClass: "idle",
    ready: false,
  },
  applied: {
    badge: "生效",
    kicker: "当前 Codex 路由",
    action: "重新应用",
    routeLabel: "当前生效",
    routeDetail: "Codex 正在使用此路由",
    readinessDetail: "保存内容与 Codex 一致",
    tone: "success",
    routeClass: "active",
    healthClass: "active",
    ready: true,
  },
  pending_changes: {
    badge: "待应用",
    kicker: "当前中转站有未应用修改",
    action: "应用修改",
    routeLabel: "有未应用修改",
    routeDetail: "保存内容尚未写入 Codex",
    readinessDetail: "保存内容尚未写入 Codex",
    tone: "warning",
    routeClass: "staged",
    healthClass: "warning",
    ready: false,
  },
  external_drift: {
    badge: "外部变更",
    kicker: "Codex 配置已被外部修改",
    action: "检查并重新应用",
    routeLabel: "外部配置变更",
    routeDetail: "Codex 配置与上次应用不一致",
    readinessDetail: "检测到外部修改，请确认后重新应用",
    tone: "error",
    routeClass: "drift",
    healthClass: "error",
    ready: false,
  },
  unknown: {
    badge: "待确认",
    kicker: "无法确认当前 Codex 路由",
    action: "重新应用",
    routeLabel: "状态未知",
    routeDetail: "无法读取或验证当前 Codex 配置",
    readinessDetail: "无法确认保存内容是否已生效",
    tone: "unknown",
    routeClass: "unknown",
    healthClass: "warning",
    ready: false,
  },
};

export function describeApplyState(state: ProfileApplyState): ApplyStatePresentation {
  return applyStatePresentations[state];
}

export function quickModelDraftState(savedModel: string, draft: string) {
  const normalized = draft.trim();
  return {
    normalized,
    dirty: normalized !== savedModel,
    valid: normalized.length > 0,
  };
}

export function filterProfiles(profiles: ProfileSummary[], query: string): ProfileSummary[] {
  const normalizedQuery = query.trim().toLowerCase();
  if (!normalizedQuery) return profiles;

  return profiles.filter((profile) =>
    [profile.name, profile.model, profile.baseUrl].some((value) =>
      value.toLowerCase().includes(normalizedQuery),
    ),
  );
}

export function routeHost(baseUrl: string) {
  if (!baseUrl) return "尚未设置";
  try {
    return new URL(baseUrl).host;
  } catch {
    return baseUrl;
  }
}

export function profileSummaryToDraft(profile: ProfileSummary): ProfileDraft {
  return {
    name: profile.name,
    baseUrl: profile.baseUrl,
    apiKey: undefined,
    clearApiKey: false,
    model: profile.model,
    reviewModel: profile.reviewModel,
  };
}
