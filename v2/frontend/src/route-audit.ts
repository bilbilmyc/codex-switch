import type {
  ModelListView,
  ProfileSummary,
  RouteAuditErrorCategory,
  RouteAuditHistoryView,
  RouteAuditSaveRequest,
  RouteAuditSavedResult,
  RouteAuditStaleReason,
} from "./types";

export type ConnectionCheck =
  | { state: "checking" }
  | {
    state: "success";
    models: ModelListView;
    latencyMs: number;
    checkedAt: number;
  }
  | { state: "error"; message: string; checkedAt: number };

type RouteAuditProfile = Pick<ProfileSummary, "id" | "name">;

export type RouteAuditHistoryMeta = {
  source: "history";
  stale: boolean;
  staleReasons: RouteAuditStaleReason[];
  staleAfterMs: number;
};

export type RouteAuditEntry = RouteAuditProfile & { history?: RouteAuditHistoryMeta } & (
  | { state: "queued" }
  | { state: "checking" }
  | { state: "success"; models?: ModelListView; modelCount?: number; latencyMs: number; checkedAt: number }
  | { state: "error"; message: string; errorCategory?: RouteAuditErrorCategory; latencyMs?: number; checkedAt: number }
  | { state: "incomplete"; issue: string; errorCategory?: RouteAuditErrorCategory; checkedAt?: number }
  | { state: "stopped"; checkedAt?: number }
);

export type RouteAuditSuccessEntry = Extract<RouteAuditEntry, { state: "success" }>;

export type RouteAuditSummary = {
  total: number;
  success: number;
  error: number;
  incomplete: number;
  stopped: number;
  pending: number;
  fastest?: RouteAuditSuccessEntry;
};

export type RouteAuditSession = {
  startedAt: number;
  finishedAt?: number;
  entries: RouteAuditEntry[];
  summary: RouteAuditSummary;
  source?: "live" | "history";
  staleAfterMs?: number;
};

export type RunRouteAuditOptions = {
  profiles: readonly ProfileSummary[];
  check: (profile: ProfileSummary) => Promise<ModelListView>;
  formatError: (error: unknown) => string;
  shouldStop?: () => boolean;
  isCurrent?: () => boolean;
  onEntry?: (entry: RouteAuditEntry, session: RouteAuditSession) => void;
  onProgress?: (session: RouteAuditSession) => void;
};

function missingProfileFields(profile: ProfileSummary) {
  const missing: Array<{ label: string; category: RouteAuditErrorCategory }> = [];

  if (!profile.baseUrl.trim()) missing.push({ label: "中转站地址", category: "missing_base_url" });
  if (!profile.hasApiKey) missing.push({ label: "API Key", category: "missing_api_key" });
  if (!profile.model.trim()) missing.push({ label: "默认模型", category: "missing_model" });

  return missing;
}

export function profileConfigurationIssue(profile: ProfileSummary): string | undefined {
  const missing = missingProfileFields(profile);

  return missing.length > 0 ? `缺少${missing.map((field) => field.label).join("、")}` : undefined;
}

export function profileConfigurationErrorCategory(profile: ProfileSummary): RouteAuditErrorCategory | undefined {
  const missing = missingProfileFields(profile);

  if (missing.length === 0) return undefined;
  return missing.length === 1 ? missing[0].category : "missing_multiple_fields";
}

export function summarizeRouteAudit(entries: readonly RouteAuditEntry[]): RouteAuditSummary {
  let success = 0;
  let error = 0;
  let incomplete = 0;
  let stopped = 0;
  let pending = 0;
  let fastest: RouteAuditSuccessEntry | undefined;

  for (const entry of entries) {
    switch (entry.state) {
      case "success":
        success += 1;
        if (!entry.history && (!fastest || entry.latencyMs < fastest.latencyMs)) fastest = entry;
        break;
      case "error":
        error += 1;
        break;
      case "incomplete":
        incomplete += 1;
        break;
      case "stopped":
        stopped += 1;
        break;
      case "queued":
      case "checking":
        pending += 1;
        break;
    }
  }

  return {
    total: entries.length,
    success,
    error,
    incomplete,
    stopped,
    pending,
    fastest,
  };
}

export function routeAuditHistoryToSession(history: RouteAuditHistoryView, profiles: readonly ProfileSummary[]): RouteAuditSession {
  const profileById = new Map(profiles.map((profile) => [profile.id, profile]));
  const seen = new Set<string>();
  const entries = history.results.map<RouteAuditEntry>((result) => {
    seen.add(result.profileId);
    const profile = profileById.get(result.profileId);
    const base: RouteAuditProfile = {
      id: result.profileId,
      name: profile?.name ?? "已删除的中转站",
    };
    const historyMeta: RouteAuditHistoryMeta = {
      source: "history",
      stale: result.stale,
      staleReasons: result.staleReasons,
      staleAfterMs: history.staleAfterMs,
    };
    const checkedAt = result.checkedAtUnixMs;
    switch (result.result) {
      case "success":
        return { ...base, state: "success", modelCount: result.modelCount, latencyMs: result.modelCheckDurationMs ?? 0, checkedAt, history: historyMeta };
      case "error":
        return { ...base, state: "error", message: result.errorMessage ?? routeAuditErrorCategoryLabel(result.errorCategory), errorCategory: result.errorCategory, latencyMs: result.modelCheckDurationMs, checkedAt, history: historyMeta };
      case "incomplete":
        return { ...base, state: "incomplete", issue: routeAuditErrorCategoryLabel(result.errorCategory), errorCategory: result.errorCategory, checkedAt, history: historyMeta };
      case "stopped":
        return { ...base, state: "stopped", checkedAt, history: historyMeta };
    }
  });
  for (const profile of profiles) {
    if (!seen.has(profile.id)) entries.push({ id: profile.id, name: profile.name, state: "queued" });
  }
  const checkedTimes = entries.flatMap((entry) => "checkedAt" in entry && entry.checkedAt ? [entry.checkedAt] : []);
  const startedAt = checkedTimes.length ? Math.min(...checkedTimes) : Date.now();
  const finishedAt = checkedTimes.length ? Math.max(...checkedTimes) : undefined;
  return {
    startedAt,
    finishedAt,
    entries,
    summary: summarizeRouteAudit(entries),
    source: "history",
    staleAfterMs: history.staleAfterMs,
  };
}

export function routeAuditSessionToSaveRequest(session: RouteAuditSession): RouteAuditSaveRequest {
  const checkedAtFallback = session.finishedAt ?? session.startedAt;
  const results: RouteAuditSavedResult[] = [];

  for (const entry of session.entries) {
    if (entry.history && (entry.history.stale || entry.history.staleReasons.includes("profile_missing"))) {
      continue;
    }
    switch (entry.state) {
      case "success":
        results.push({
          profileId: entry.id,
          result: "success",
          modelCount: entry.modelCount ?? entry.models?.models.length ?? 0,
          modelCheckDurationMs: entry.latencyMs,
          checkedAtUnixMs: entry.checkedAt,
        });
        break;
      case "error":
        results.push({
          profileId: entry.id,
          result: "error",
          modelCheckDurationMs: entry.latencyMs,
          checkedAtUnixMs: entry.checkedAt,
          errorCategory: entry.errorCategory ?? "model_request_failed",
        });
        break;
      case "incomplete":
        results.push({
          profileId: entry.id,
          result: "incomplete",
          checkedAtUnixMs: entry.checkedAt ?? checkedAtFallback,
          errorCategory: entry.errorCategory ?? "unknown",
        });
        break;
      case "stopped":
        results.push({
          profileId: entry.id,
          result: "stopped",
          checkedAtUnixMs: entry.checkedAt ?? checkedAtFallback,
        });
        break;
      case "queued":
      case "checking":
        break;
    }
  }

  return {
    results,
  };
}

export function mergeRouteAuditConnection(session: RouteAuditSession, profile: RouteAuditProfile, connection: ConnectionCheck): RouteAuditSession {
  const nextEntry: RouteAuditEntry = connection.state === "success"
    ? { ...profile, ...connection, modelCount: connection.models.models.length }
    : connection.state === "error"
      ? { ...profile, ...connection, errorCategory: "model_request_failed" }
      : { ...profile, ...connection };
  const entries = session.entries.map((entry) => entry.id === profile.id ? nextEntry : entry);
  const finishedAt = connection.state === "checking" ? session.finishedAt : Date.now();
  return {
    ...session,
    source: session.source ?? "live",
    finishedAt,
    entries,
    summary: summarizeRouteAudit(entries),
  };
}

export function routeAuditHistoryHasStale(session?: RouteAuditSession) {
  return routeAuditHistoryRefreshCount(session) > 0;
}

export function routeAuditEntryNeedsRefresh(entry: RouteAuditEntry, historicalSession: boolean) {
  return Boolean(entry.history?.stale || (historicalSession && entry.state === "queued"));
}

export function routeAuditHistoryRefreshCount(session?: RouteAuditSession) {
  if (!session || session.source !== "history") return 0;
  return session.entries.filter((entry) => routeAuditEntryNeedsRefresh(entry, true)).length;
}

export function canApplyRouteAuditEntry(entry: RouteAuditEntry) {
  return entry.state === "success" && !entry.history;
}

export function routeAuditStaleReasonLabel(reason: RouteAuditStaleReason, staleAfterMs: number) {
  if (reason === "profile_changed") return "配置已变";
  if (reason === "profile_missing") return "中转站已删除";
  if (reason === "unverifiable") return "无法验证配置版本";
  const hours = Math.max(1, Math.round(staleAfterMs / 3_600_000));
  return `超过 ${hours} 小时`;
}

export function routeAuditErrorCategoryLabel(category?: RouteAuditErrorCategory) {
  if (category === "missing_base_url") return "缺少中转站地址";
  if (category === "missing_api_key") return "缺少 API Key";
  if (category === "missing_model") return "缺少默认模型";
  if (category === "missing_multiple_fields") return "缺少多项必要配置";
  if (category === "model_request_failed") return "模型目录请求失败";
  return "连接检查失败";
}

export async function runRouteAudit(options: RunRouteAuditOptions): Promise<RouteAuditSession> {
  const {
    profiles,
    check,
    formatError,
    shouldStop = () => false,
    isCurrent = () => true,
    onEntry,
    onProgress,
  } = options;
  const startedAt = Date.now();
  let entries: RouteAuditEntry[] = profiles.map((profile) => {
    const issue = profileConfigurationIssue(profile);
    return issue
      ? { id: profile.id, name: profile.name, state: "incomplete", issue, errorCategory: profileConfigurationErrorCategory(profile) }
      : { id: profile.id, name: profile.name, state: "queued" };
  });

  const session = (finishedAt?: number): RouteAuditSession => ({
    startedAt,
    finishedAt,
    entries: [...entries],
    summary: summarizeRouteAudit(entries),
    source: "live",
  });
  const emitEntry = (entry: RouteAuditEntry) => {
    if (!isCurrent()) return false;
    onEntry?.(entry, session());
    return true;
  };
  const emitProgress = () => {
    if (!isCurrent()) return false;
    onProgress?.(session());
    return true;
  };
  const replaceEntry = (index: number, entry: RouteAuditEntry) => {
    entries = entries.map((current, currentIndex) => (currentIndex === index ? entry : current));
  };

  if (isCurrent()) {
    for (const entry of entries) onEntry?.(entry, session());
    onProgress?.(session());
  }

  for (let index = 0; index < profiles.length; index += 1) {
    if (!isCurrent()) return session();
    if (entries[index].state !== "queued") continue;

    if (shouldStop()) {
      for (let stoppedIndex = index; stoppedIndex < entries.length; stoppedIndex += 1) {
        if (entries[stoppedIndex].state !== "queued") continue;
        const stoppedEntry: RouteAuditEntry = {
          id: profiles[stoppedIndex].id,
          name: profiles[stoppedIndex].name,
          state: "stopped",
        };
        replaceEntry(stoppedIndex, stoppedEntry);
        if (!emitEntry(stoppedEntry)) return session();
      }
      emitProgress();
      const finishedAt = Date.now();
      const finished = session(finishedAt);
      if (isCurrent()) onProgress?.(finished);
      return finished;
    }

    const profile = profiles[index];
    const checkingEntry: RouteAuditEntry = {
      id: profile.id,
      name: profile.name,
      state: "checking",
    };
    replaceEntry(index, checkingEntry);
    if (!emitEntry(checkingEntry)) return session();
    if (!emitProgress()) return session();

    const requestStartedAt = performance.now();
    let resultEntry: RouteAuditEntry;
    try {
      const models = await check(profile);
      const latencyMs = Math.max(0, Math.round(performance.now() - requestStartedAt));
        resultEntry = {
        id: profile.id,
        name: profile.name,
        state: "success",
          models,
          modelCount: models.models.length,
        latencyMs,
        checkedAt: Date.now(),
      };
    } catch (error) {
      resultEntry = {
        id: profile.id,
        name: profile.name,
        state: "error",
        message: formatError(error),
        errorCategory: "model_request_failed",
        checkedAt: Date.now(),
      };
    }

    replaceEntry(index, resultEntry);
    if (!emitEntry(resultEntry)) return session();
    if (!emitProgress()) return session();
  }

  const finished = session(Date.now());
  if (isCurrent()) onProgress?.(finished);
  return finished;
}
