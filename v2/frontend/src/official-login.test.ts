import { describe, expect, it, vi } from "vitest";

describe("official login switch", () => {
  it("keeps saved relays and credentials available for switching back", async () => {
    vi.resetModules();
    const { api } = await import("./api");
    const before = await api.bootstrap();
    const relay = before.profiles[0];
    expect(await api.prepareOfficialLogin()).toEqual({ kind: "official_login_restored" });
    const official = await api.bootstrap();
    expect(official.profiles).toEqual(before.profiles.map((profile) => ({
      ...profile,
      isActive: false,
      applyState: "inactive",
    })));
    expect(await api.checkApplied(relay.id)).toBe(false);
    expect((await api.loadContext(relay.id)).isActive).toBe(false);
    await api.prepareApply(relay.id);
    expect(await api.checkApplied(relay.id)).toBe(true);
    expect((await api.bootstrap()).profiles[0].hasApiKey).toBe(true);
  });
});
