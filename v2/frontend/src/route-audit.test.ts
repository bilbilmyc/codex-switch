import { describe, expect, it, vi } from "vitest";
import {
  profileConfigurationIssue,
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
});
