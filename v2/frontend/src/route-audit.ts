import type { ModelListView, ProfileSummary } from "./types";

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

export type RouteAuditEntry = RouteAuditProfile & (
  | { state: "queued" }
  | ConnectionCheck
  | { state: "incomplete"; issue: string }
  | { state: "stopped" }
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

export function profileConfigurationIssue(profile: ProfileSummary): string | undefined {
  const missing: string[] = [];

  if (!profile.baseUrl.trim()) missing.push("中转站地址");
  if (!profile.hasApiKey) missing.push("API Key");
  if (!profile.model.trim()) missing.push("默认模型");

  return missing.length > 0 ? `缺少${missing.join("、")}` : undefined;
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
        if (!fastest || entry.latencyMs < fastest.latencyMs) fastest = entry;
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
      ? { id: profile.id, name: profile.name, state: "incomplete", issue }
      : { id: profile.id, name: profile.name, state: "queued" };
  });

  const session = (finishedAt?: number): RouteAuditSession => ({
    startedAt,
    finishedAt,
    entries: [...entries],
    summary: summarizeRouteAudit(entries),
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
        latencyMs,
        checkedAt: Date.now(),
      };
    } catch (error) {
      resultEntry = {
        id: profile.id,
        name: profile.name,
        state: "error",
        message: formatError(error),
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
