import { describe, expect, it } from "vitest";
import { buildBackupDiffRows, formatBackupLocalTime } from "./BackupCenterDialog";
import type { BackupPreview, BackupRouteSnapshot } from "./types";

describe("backup center presentation", () => {
  it("formats snapshot timestamps as absolute local time", () => {
    const localTime = new Date(2026, 8, 4, 9, 5).getTime();

    expect(formatBackupLocalTime(localTime)).toBe("2026-09-04 09:05");
  });

  it("never exposes an accidental URL or credential value in managed diff rows", () => {
    const current = {
      providerName: "Current relay",
      baseUrlConfigured: true,
      model: "gpt-current",
      contextSummary: "Current context",
      hasApiKey: true,
      baseUrl: "https://private-current.example/v1",
      apiKey: "current-sensitive-value",
    } as BackupRouteSnapshot & { baseUrl: string; apiKey: string };
    const target = {
      providerName: "Target relay",
      baseUrlConfigured: true,
      model: "gpt-target",
      contextSummary: "Target context",
      hasApiKey: true,
      baseUrl: "https://private-target.example/v1",
      apiKey: "target-sensitive-value",
    } as BackupRouteSnapshot & { baseUrl: string; apiKey: string };
    const preview: BackupPreview = {
      backup: {
        id: "backup",
        createdAtUnixMs: 1,
        configPresent: true,
        authPresent: true,
        statePresent: true,
        catalogPresent: true,
      },
      liveRevision: "revision",
      current,
      target,
      managedChanges: {
        provider: "changed",
        baseUrl: "changed",
        model: "changed",
        reviewModel: "unchanged",
        context: "changed",
        apiKey: "replaced",
      },
      fileChanges: { config: true, auth: true, state: true, catalog: true },
    };

    const rendered = JSON.stringify(buildBackupDiffRows(preview));

    expect(rendered).not.toContain("private-current");
    expect(rendered).not.toContain("private-target");
    expect(rendered).not.toContain("sensitive-value");
    expect(rendered).toContain("已配置");
    expect(rendered).toContain("将恢复设置");
    expect(rendered).toContain("将替换");
  });
});
