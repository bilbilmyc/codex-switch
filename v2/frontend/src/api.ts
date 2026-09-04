import { invoke } from "@tauri-apps/api/core";
import type {
  ApplyResponse,
  BackupCenterView,
  BackupPreview,
  Bootstrap,
  ContextDraft,
  ContextView,
  ModelListView,
  ProfileDraft,
  ProfileSummary,
  UsageView,
} from "./types";

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let previewProfiles: ProfileSummary[] = [
  {
    id: "preview-relay",
    name: "团队中转站",
    baseUrl: "https://relay.example.com/v1",
    model: "gpt-5.2-codex",
    reviewModel: "gpt-5.2",
    hasApiKey: true,
    isActive: true,
    applyState: "applied",
  },
];

let previewLiveProfile = { ...previewProfiles[0] };

const previewBackups: BackupCenterView["backups"] = [
  {
    id: "preview-backup-current",
    createdAtUnixMs: new Date(2026, 8, 4, 18, 42).getTime(),
    configPresent: true,
    authPresent: true,
    statePresent: true,
    catalogPresent: true,
  },
  {
    id: "preview-backup-previous",
    createdAtUnixMs: new Date(2026, 8, 3, 23, 10).getTime(),
    configPresent: true,
    authPresent: false,
    statePresent: true,
    catalogPresent: false,
  },
];

const previewBackupPreviews: Record<string, BackupPreview> = {
  "preview-backup-current": {
    backup: previewBackups[0],
    liveRevision: "preview-live-r1",
    current: {
      providerName: "团队中转站",
      baseUrlConfigured: true,
      model: "gpt-5.2-codex",
      reviewModel: "gpt-5.2",
      contextSummary: "272K 窗口 · 输出不限 · 压缩 80%",
      hasApiKey: true,
    },
    target: {
      providerName: "稳定路由",
      baseUrlConfigured: true,
      model: "gpt-5.1-codex",
      contextSummary: "自动窗口 · 输出不限 · 自动压缩",
      hasApiKey: true,
    },
    managedChanges: {
      provider: "changed",
      baseUrl: "changed",
      model: "changed",
      reviewModel: "changed",
      context: "changed",
      apiKey: "replaced",
    },
    fileChanges: { config: true, auth: true, state: true, catalog: true },
    activeProfile: { id: "preview-relay", name: "稳定路由" },
  },
  "preview-backup-previous": {
    backup: previewBackups[1],
    liveRevision: "preview-live-r1",
    current: {
      providerName: "团队中转站",
      baseUrlConfigured: true,
      model: "gpt-5.2-codex",
      reviewModel: "gpt-5.2",
      contextSummary: "272K 窗口 · 输出不限 · 压缩 80%",
      hasApiKey: true,
    },
    target: {
      providerName: "旧中转站",
      baseUrlConfigured: true,
      model: "gpt-5",
      contextSummary: "200K 窗口 · 输出不限 · 压缩 80%",
      hasApiKey: false,
    },
    managedChanges: {
      provider: "changed",
      baseUrl: "changed",
      model: "changed",
      reviewModel: "changed",
      context: "changed",
      apiKey: "removed",
    },
    fileChanges: { config: true, auth: true, state: true, catalog: true },
  },
};

function toSummary(id: string, draft: ProfileDraft, isActive = false): ProfileSummary {
  return {
    id,
    name: draft.name,
    baseUrl: draft.baseUrl,
    model: draft.model,
    reviewModel: draft.reviewModel,
    hasApiKey: Boolean(draft.apiKey),
    isActive,
    applyState: "inactive",
  };
}

function previewApplyState(
  profile: ProfileSummary,
  credentialChanged = false,
): ProfileSummary["applyState"] {
  if (profile.id !== previewLiveProfile.id) return "inactive";
  if (credentialChanged) return "pending_changes";
  return profile.name === previewLiveProfile.name
    && profile.baseUrl === previewLiveProfile.baseUrl
    && profile.model === previewLiveProfile.model
    && profile.reviewModel === previewLiveProfile.reviewModel
    && profile.hasApiKey === previewLiveProfile.hasApiKey
    ? "applied"
    : "pending_changes";
}

export const api = {
  async bootstrap(): Promise<Bootstrap> {
    if (isTauri) return invoke<Bootstrap>("bootstrap");
    return { profiles: previewProfiles, canRestore: false };
  },
  async createProfile(draft: ProfileDraft): Promise<ProfileSummary> {
    if (isTauri) return invoke<ProfileSummary>("create_profile", { draft });
    const profile = toSummary(`preview-${crypto.randomUUID()}`, draft);
    previewProfiles = [...previewProfiles, profile];
    return profile;
  },
  async newProfile(): Promise<ProfileSummary> {
    if (isTauri) return invoke<ProfileSummary>("new_profile");
    const profile = toSummary(`preview-${crypto.randomUUID()}`, {
      name: "新中转站",
      baseUrl: "https://relay.example/v1",
      apiKey: "",
      model: "gpt-5",
    });
    previewProfiles = [...previewProfiles, profile];
    return profile;
  },
  async updateProfile(profileId: string, draft: ProfileDraft): Promise<ProfileSummary> {
    if (isTauri) return invoke<ProfileSummary>("update_profile", { profileId, draft });
    const previous = previewProfiles.find((profile) => profile.id === profileId);
    const profile = {
      ...toSummary(profileId, draft, previous?.isActive),
      hasApiKey: draft.clearApiKey ? false : Boolean(draft.apiKey) || Boolean(previous?.hasApiKey),
    };
    profile.applyState = previewApplyState(profile, Boolean(draft.apiKey?.trim()));
    previewProfiles = previewProfiles.map((item) => (item.id === profileId ? profile : item));
    return profile;
  },
  async duplicateProfile(profileId: string): Promise<ProfileSummary> {
    if (isTauri) return invoke<ProfileSummary>("duplicate_profile", { profileId });
    const source = previewProfiles.find((profile) => profile.id === profileId)!;
    const copy = {
      ...source,
      id: `preview-${crypto.randomUUID()}`,
      name: `${source.name} 副本`,
      isActive: false,
      applyState: "inactive" as const,
    };
    previewProfiles = [...previewProfiles, copy];
    return copy;
  },
  async deleteProfile(profileId: string): Promise<void> {
    if (isTauri) return invoke<void>("delete_profile", { profileId });
    previewProfiles = previewProfiles.filter((profile) => profile.id !== profileId);
  },
  async importProfiles(): Promise<Bootstrap> {
    if (isTauri) return invoke<Bootstrap>("import_profiles");
    return { profiles: previewProfiles, canRestore: false };
  },
  async importCurrent(): Promise<ProfileSummary> {
    if (isTauri) return invoke<ProfileSummary>("import_current");
    return previewProfiles[0];
  },
  async exportProfiles(includeKeys: boolean): Promise<void> {
    if (isTauri) return invoke<void>("export_profiles", { includeKeys });
  },
  async loadModelCache(profileId: string): Promise<ModelListView> {
    if (isTauri) return invoke<ModelListView>("load_model_cache", { profileId });
    const profile = previewProfiles.find((item) => item.id === profileId);
    return { models: profile ? [profile.model] : [], cacheLabel: "尚未获取模型列表" };
  },
  async refreshModels(profileId: string, draft: ProfileDraft): Promise<ModelListView> {
    if (isTauri) return invoke<ModelListView>("refresh_models", { profileId, draft });
    const models = [...new Set([draft.model, "glm-5.3", "deepseek-v4-flash", "qwen3.8-max"].filter(Boolean))];
    return { models, cacheLabel: `刚刚获取了 ${models.length} 个模型` };
  },
  async loadBackupCenter(): Promise<BackupCenterView> {
    if (isTauri) return invoke<BackupCenterView>("load_backup_center");
    return { backups: previewBackups };
  },
  async loadBackupPreview(backupId: string): Promise<BackupPreview> {
    if (isTauri) return invoke<BackupPreview>("load_backup_preview", { backupId });
    const preview = previewBackupPreviews[backupId];
    if (!preview) throw new Error("这个预览快照不存在");
    return preview;
  },
  async prepareBackupRestore(backupId: string, liveRevision: string): Promise<ApplyResponse> {
    if (isTauri) return invoke<ApplyResponse>("prepare_backup_restore", { backupId, liveRevision });
    return {
      kind: "restored",
      activeProfileId: previewProfiles.find((profile) => profile.isActive)?.id,
    };
  },
  async loadContext(profileId: string): Promise<ContextView> {
    if (isTauri) return invoke<ContextView>("load_context", { profileId });
    return {
      useDefaults: true,
      windowK: "",
      compactPercent: 80,
      summary: "自动窗口 · 输出不限 · 自动压缩",
      isActive: true,
      syncState: "synced",
      status: "上下文配置 · 已同步到 Codex",
      budget: {
        recentSession: "暂无记录",
        instructionTokens: "暂无记录",
        availableBudget: "自动",
        historyRatio: 0,
        instructionRatio: 0,
        remainingRatio: 1,
        suggestedWindowK: "272",
        instructions: [],
      },
    };
  },
  async saveContext(profileId: string, draft: ContextDraft): Promise<ApplyResponse> {
    if (isTauri) return invoke<ApplyResponse>("save_context", { profileId, draft });
    return {
      kind: "context_saved",
      context: {
        ...draft,
        summary: draft.useDefaults
          ? "自动窗口 · 输出不限 · 自动压缩"
          : `${draft.windowK}K 窗口 · 输出不限 · 压缩 ${draft.compactPercent}%`,
        isActive: true,
        syncState: "synced",
        status: "上下文配置 · 已同步到 Codex",
        budget: {
          recentSession: "暂无记录",
          instructionTokens: "暂无记录",
          availableBudget: "自动",
          historyRatio: 0,
          instructionRatio: 0,
          remainingRatio: 1,
          suggestedWindowK: "272",
          instructions: [],
        },
      },
    };
  },
  async refreshUsage(profileId: string, period: string): Promise<UsageView> {
    if (isTauri) return invoke<UsageView>("refresh_usage", { profileId, period });
    const zero = { input: "0", cached: "0", output: "0", calls: "0 次" };
    return { period: period as UsageView["period"], current: zero, todaySummary: "今日暂无本地记录", periodTotal: zero, trend: [], models: [], hasData: false, status: "暂无用量数据" };
  },
  async exportUsage(profileId: string, period: string): Promise<void> {
    if (isTauri) return invoke<void>("export_usage", { profileId, period });
  },
  async prepareApply(profileId: string): Promise<ApplyResponse> {
    if (isTauri) return invoke<ApplyResponse>("prepare_apply", { profileId });
    const target = previewProfiles.find((profile) => profile.id === profileId);
    if (target) previewLiveProfile = { ...target, isActive: true, applyState: "applied" };
    previewProfiles = previewProfiles.map((profile) => ({
      ...profile,
      isActive: profile.id === profileId,
      applyState: profile.id === profileId ? "applied" : "inactive",
    }));
    return { kind: "applied", activeProfileId: profileId };
  },
  async continueApply(token: string, choice: string): Promise<ApplyResponse> {
    if (isTauri) return invoke<ApplyResponse>("continue_apply", { token, choice });
    previewProfiles = previewProfiles.map((profile) => ({
      ...profile,
      isActive: profile.id === "preview-relay",
      applyState: profile.id === "preview-relay" ? "applied" : "inactive",
    }));
    const target = previewProfiles.find((profile) => profile.id === "preview-relay");
    if (target) previewLiveProfile = { ...target };
    return { kind: "applied", activeProfileId: "preview-relay" };
  },
  async dismissConfirmation(token: string): Promise<void> {
    if (isTauri) return invoke<void>("dismiss_confirmation", { token });
  },
};
