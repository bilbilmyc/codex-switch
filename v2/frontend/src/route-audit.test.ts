import { describe, expect, it, vi } from "vitest";
import {
  canApplyRouteAuditEntry,
  mergeRouteAuditConnection,
  profileConfigurationIssue,
  routeAuditHistoryRefreshCount,
  routeAuditHistoryToSession,
  routeAuditSessionToSaveRequest,
  routeAuditStaleReasonLabel,
  runRouteAudit,
  summarizeRouteAudit,
  type RouteAuditEntry,
} from "./route-audit";
import type { ModelListView, ProfileSummary } from "./types";

function profile(id: string, overrides: Partial<ProfileSummary> = {}): ProfileSummary {
  return {
    id,
    name: `Relay ${id}`,
    baseUrl: `https://${id}.example.com/v1`,
    model: "gpt-5.6-sol",
    hasApiKey: true,
    isActive: false,
    applyState: "inactive",
    ...overrides,
  };
}

function modelList(model: string): ModelListView {
  return { models: [model], cacheLabel: `1 model from ${model}` };
}

describe("route audit", () => {
  it("reports every missing configuration field and skips incomplete profiles", async () => {
    const incomplete = profile("incomplete", {
      baseUrl: "  ",
      model: "",
      hasApiKey: false,
    });
    const ready = profile("ready");
    const check = vi.fn(async (target: ProfileSummary) => modelList(target.model));
    const initialEntries: RouteAuditEntry[] = [];

    expect(profileConfigurationIssue(incomplete)).toBe("缺少中转站地址、API Key、默认模型");

    const session = await runRouteAudit({
      profiles: [incomplete, ready],
      check,
      formatError: String,
      onEntry: (entry) => {
        if (entry.state === "incomplete" || entry.state === "queued") initialEntries.push(entry);
      },
    });

    expect(initialEntries.slice(0, 2).map((entry) => entry.state)).toEqual(["incomplete", "queued"]);
    expect(check).toHaveBeenCalledOnce();
    expect(check).toHaveBeenCalledWith(ready);
    expect(session.entries[0]).toMatchObject({ state: "incomplete" });
    expect(session.entries[0]).toMatchObject({ errorCategory: "missing_multiple_fields" });
    expect(session.entries[1]).toMatchObject({ state: "success", models: modelList(ready.model) });
    expect(session.summary).toMatchObject({ success: 1, incomplete: 1, pending: 0 });
  });

  it("checks routes serially, merges results, formats errors, and finds the fastest success", async () => {
    const profiles = [profile("one"), profile("two"), profile("three")];
    const calls: string[] = [];
    let active = 0;
    let maxActive = 0;
    const check = vi.fn(async (target: ProfileSummary) => {
      calls.push(`start:${target.id}`);
      active += 1;
      maxActive = Math.max(maxActive, active);
      await Promise.resolve();
      active -= 1;
      calls.push(`end:${target.id}`);
      if (target.id === "two") throw new Error("upstream unavailable");
      return modelList(target.model);
    });
    const formatError = vi.fn((error: unknown) => `formatted: ${(error as Error).message}`);

    const session = await runRouteAudit({ profiles, check, formatError });

    expect(maxActive).toBe(1);
    expect(calls).toEqual([
      "start:one",
      "end:one",
      "start:two",
      "end:two",
      "start:three",
      "end:three",
    ]);
    expect(session.entries.map((entry) => entry.state)).toEqual(["success", "error", "success"]);
    expect(session.entries[0]).toMatchObject({ models: modelList("gpt-5.6-sol") });
    expect(session.entries[1]).toMatchObject({ message: "formatted: upstream unavailable" });
    expect(session.entries[1]).toMatchObject({ errorCategory: "model_request_failed" });
    expect(formatError).toHaveBeenCalledOnce();
    expect(session.summary).toMatchObject({ success: 2, error: 1, pending: 0 });
    expect(session.summary.fastest?.state).toBe("success");
    expect(session.summary.fastest?.latencyMs).toBeLessThanOrEqual(
      (session.entries[2] as Extract<RouteAuditEntry, { state: "success" }>).latencyMs,
    );
  });

  it("stops after the current request and does not start the next route", async () => {
    const profiles = [profile("one"), profile("two"), profile("three")];
    let stopRequested = false;
    const check = vi.fn(async () => {
      stopRequested = true;
      return modelList("gpt-5.6-sol");
    });

    const session = await runRouteAudit({
      profiles,
      check,
      formatError: String,
      shouldStop: () => stopRequested,
    });

    expect(check).toHaveBeenCalledOnce();
    expect(session.entries.map((entry) => entry.state)).toEqual(["success", "stopped", "stopped"]);
    expect(session.summary).toMatchObject({ success: 1, stopped: 2, pending: 0 });
  });

  it("does not publish an old run result or start another request once it is no longer current", async () => {
    const profiles = [profile("one"), profile("two")];
    let current = true;
    let resolveFirst!: (value: ModelListView) => void;
    const firstResult = new Promise<ModelListView>((resolve) => {
      resolveFirst = resolve;
    });
    const check = vi.fn(() => firstResult);
    const emitted: RouteAuditEntry[] = [];
    const audit = runRouteAudit({
      profiles,
      check,
      formatError: String,
      isCurrent: () => current,
      onEntry: (entry) => emitted.push(entry),
    });

    await vi.waitFor(() => expect(check).toHaveBeenCalledOnce());
    current = false;
    resolveFirst(modelList("gpt-5.6-sol"));
    await audit;

    expect(check).toHaveBeenCalledOnce();
    expect(emitted.some((entry) => entry.state === "success" || entry.state === "error")).toBe(false);
  });

  it("counts pending states and keeps the first success when latencies tie", () => {
    const entries: RouteAuditEntry[] = [
      { id: "queued", name: "Queued", state: "queued" },
      { id: "checking", name: "Checking", state: "checking" },
      {
        id: "fast",
        name: "Fast",
        state: "success",
        models: modelList("gpt-5.6-sol"),
        latencyMs: 12,
        checkedAt: 1,
      },
      {
        id: "tie",
        name: "Tie",
        state: "success",
        models: modelList("glm-5.3"),
        latencyMs: 12,
        checkedAt: 2,
      },
    ];

    expect(summarizeRouteAudit(entries)).toMatchObject({
      total: 4,
      success: 2,
      pending: 2,
      fastest: entries[2],
    });
  });

  it("converts persisted summaries without reconstructing models or credentials", () => {
    const session = routeAuditHistoryToSession({
      staleAfterMs: 86_400_000,
      results: [
        {
          profileId: "one",
          result: "success",
          modelCount: 3,
          modelCheckDurationMs: 24,
          checkedAtUnixMs: 10,
          stale: true,
          staleReasons: ["profile_changed"],
        },
        {
          profileId: "removed",
          result: "error",
          checkedAtUnixMs: 11,
          errorMessage: "Safe summary",
          stale: true,
          staleReasons: ["profile_missing"],
        },
      ],
    }, [profile("one"), profile("new")]);

    expect(session.source).toBe("history");
    expect(session.entries[0]).toMatchObject({ state: "success", modelCount: 3, history: { stale: true } });
    expect(session.entries[0]).not.toHaveProperty("models");
    expect(session.entries[1]).toMatchObject({ name: "已删除的中转站", state: "error", message: "Safe summary" });
    expect(session.entries[2]).toMatchObject({ id: "new", state: "queued" });
    expect(routeAuditHistoryRefreshCount(session)).toBe(3);
    expect(routeAuditStaleReasonLabel("expired", session.staleAfterMs!)).toBe("超过 24 小时");

    const retried = mergeRouteAuditConnection(session, profile("one"), {
      state: "success",
      models: modelList("gpt-5.6-sol"),
      latencyMs: 12,
      checkedAt: 30,
    });
    expect(retried.source).toBe("history");
    expect(routeAuditHistoryRefreshCount(retried)).toBe(2);
  });

  it("does not persist stale history as a fresh result", () => {
    const session = routeAuditHistoryToSession({
      staleAfterMs: 86_400_000,
      results: [{
        profileId: "one",
        result: "error",
        checkedAtUnixMs: 20,
        errorCategory: "model_request_failed",
        errorMessage: "Do not save this message",
        stale: true,
        staleReasons: ["expired"],
      }],
    }, [profile("one")]);

    const saved = routeAuditSessionToSaveRequest(session);

    expect(saved).toEqual({ results: [] });
    expect(JSON.stringify(saved)).not.toContain("Do not save this message");
    expect(JSON.stringify(saved)).not.toContain("stale");
    expect(JSON.stringify(saved)).not.toContain("models");
  });

  it("keeps fresh history and live retries while preserving only redacted fields", () => {
    const session = routeAuditHistoryToSession({
      staleAfterMs: 86_400_000,
      results: [{
        profileId: "fresh",
        result: "error",
        checkedAtUnixMs: 20,
        errorCategory: "model_request_failed",
        errorMessage: "Do not save this message",
        stale: false,
        staleReasons: [],
      }],
    }, [profile("fresh")]);
    session.entries.push({
      id: "live",
      name: "Live retry",
      state: "success",
      models: modelList("secret-model"),
      modelCount: 1,
      latencyMs: 14,
      checkedAt: 30,
    });

    const saved = routeAuditSessionToSaveRequest(session);

    expect(saved).toEqual({ results: [
      { profileId: "fresh", result: "error", checkedAtUnixMs: 20, errorCategory: "model_request_failed" },
      { profileId: "live", result: "success", modelCount: 1, modelCheckDurationMs: 14, checkedAtUnixMs: 30 },
    ] });
    expect(JSON.stringify(saved)).not.toContain("Do not save this message");
    expect(JSON.stringify(saved)).not.toContain("secret-model");
  });

  it("uses the finished time for incomplete and stopped entries", () => {
    const session = {
      startedAt: 100,
      finishedAt: 200,
      entries: [
        { id: "incomplete", name: "Incomplete", state: "incomplete" as const, issue: "缺少 API Key", errorCategory: "missing_api_key" as const },
        { id: "stopped", name: "Stopped", state: "stopped" as const },
      ],
      summary: { total: 2, success: 0, error: 0, incomplete: 1, stopped: 1, pending: 0 },
      source: "live" as const,
    };

    expect(routeAuditSessionToSaveRequest(session)).toEqual({
      results: [
        { profileId: "incomplete", result: "incomplete", checkedAtUnixMs: 200, errorCategory: "missing_api_key" },
        { profileId: "stopped", result: "stopped", checkedAtUnixMs: 200 },
      ],
    });
    expect(routeAuditStaleReasonLabel("unverifiable", 86_400_000)).toBe("无法验证配置版本");
  });

  it("never offers live apply actions for historical results", () => {
    const live: RouteAuditEntry = { id: "live", name: "Live", state: "success", models: modelList("gpt-5.6-sol"), latencyMs: 12, checkedAt: 1 };
    const historical: RouteAuditEntry = {
      id: "history",
      name: "History",
      state: "success",
      modelCount: 2,
      latencyMs: 12,
      checkedAt: 1,
      history: { source: "history", stale: false, staleReasons: [], staleAfterMs: 86_400_000 },
    };

    expect(canApplyRouteAuditEntry(live)).toBe(true);
    expect(canApplyRouteAuditEntry(historical)).toBe(false);
  });
});
