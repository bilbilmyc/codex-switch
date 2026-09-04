import * as Dialog from "@radix-ui/react-dialog";
import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Check,
  CircleAlert,
  Copy,
  Download,
  History,
  Info,
  Pencil,
  Plus,
  RefreshCw,
  Save,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useForm } from "react-hook-form";
import { api } from "./api";
import {
  emptyProfileForm,
  profileSchema,
  toProfileDraft,
  type ProfileFormValues,
} from "./profile-draft";
import type {
  ApplyResponse,
  Confirmation,
  ConfirmationIntent,
  ContextDraft,
  ContextView,
  ModelListView,
  ProfileDraft,
  ProfileSummary,
  UsageView,
} from "./types";

type Page = "relay" | "context" | "usage";
type Notice = { tone: "success" | "warning" | "error"; text: string } | null;
type ConfirmAction = "delete" | "restore" | "export" | null;
type PendingSelection = { profileId: string } | null;

const defaultContext: ContextDraft = {
  useDefaults: true,
  windowK: "",
  compactPercent: 80,
};

function isTauriRuntime() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export default function App() {
  const queryClient = useQueryClient();
  const bootstrap = useQuery({ queryKey: ["bootstrap"], queryFn: api.bootstrap });
  const [selectedId, setSelectedId] = useState<string>();
  const [page, setPage] = useState<Page>("relay");
  const [editorProfile, setEditorProfile] = useState<ProfileSummary>();
  const [confirmation, setConfirmation] = useState<Confirmation>();
  const [confirmAction, setConfirmAction] = useState<ConfirmAction>(null);
  const [pendingSelection, setPendingSelection] = useState<PendingSelection>(null);
  const [deferredSelectionId, setDeferredSelectionId] = useState<string>();
  const [notice, setNotice] = useState<Notice>(null);
  const [contextDraft, setContextDraft] = useState<ContextDraft>(defaultContext);
  const [contextDirty, setContextDirty] = useState(false);
  const [profileDirty, setProfileDirty] = useState(false);
  const [windowCloseContext, setWindowCloseContext] = useState(false);
  const [windowCloseEditorRequest, setWindowCloseEditorRequest] = useState(0);
  const [closeAfterContextSave, setCloseAfterContextSave] = useState(false);
  const [usagePeriod, setUsagePeriod] = useState<UsageView["period"]>("today");
  const allowWindowClose = useRef(false);

  const profiles = bootstrap.data?.profiles ?? [];
  const selectedProfile = useMemo(
    () => profiles.find((profile) => profile.id === selectedId) ?? profiles[0],
    [profiles, selectedId],
  );
  const context = useQuery({
    queryKey: ["context", selectedProfile?.id],
    queryFn: () => api.loadContext(selectedProfile!.id),
    enabled: Boolean(selectedProfile),
  });
  const modelCache = useQuery({
    queryKey: ["model-cache", selectedProfile?.id],
    queryFn: () => api.loadModelCache(selectedProfile!.id),
    enabled: Boolean(selectedProfile),
  });
  const usage = useQuery({
    queryKey: ["usage", selectedProfile?.id, usagePeriod],
    queryFn: () => api.refreshUsage(selectedProfile!.id, usagePeriod),
    enabled: Boolean(selectedProfile),
    refetchInterval: 900_000,
  });
  const todayUsage = useQuery({
    queryKey: ["usage", selectedProfile?.id, "today"],
    queryFn: () => api.refreshUsage(selectedProfile!.id, "today"),
    enabled: Boolean(selectedProfile),
    refetchInterval: 900_000,
  });

  useEffect(() => {
    if (profiles.length > 0 && !profiles.some((profile) => profile.id === selectedId)) {
      setSelectedId(profiles.find((profile) => profile.isActive)?.id ?? profiles[0].id);
    }
  }, [profiles, selectedId]);

  useEffect(() => {
    if (context.data && !contextDirty) {
      setContextDraft({
        useDefaults: context.data.useDefaults,
        windowK: context.data.windowK,
        compactPercent: context.data.compactPercent,
      });
    }
  }, [context.data, contextDirty]);

  useEffect(() => {
    if (bootstrap.data?.startupMessage) {
      setNotice({ tone: "warning", text: bootstrap.data.startupMessage });
    }
  }, [bootstrap.data?.startupMessage]);

  const refresh = async () => queryClient.invalidateQueries({ queryKey: ["bootstrap"] });
  const refreshContext = async () =>
    queryClient.invalidateQueries({ queryKey: ["context", selectedProfile?.id] });
  const refreshModelCache = async (profileId?: string) =>
    queryClient.invalidateQueries({ queryKey: ["model-cache", profileId ?? selectedProfile?.id] });
  const closeWindow = useCallback(() => {
    allowWindowClose.current = true;
    if (!isTauriRuntime()) {
      window.close();
      return;
    }
    void getCurrentWindow().close().catch(() => {
      allowWindowClose.current = false;
      setNotice({ tone: "error", text: "窗口未能关闭，请重试" });
    });
  }, []);

  const newProfile = useMutation({
    mutationFn: api.newProfile,
    onSuccess: async (profile) => {
      await refresh();
      setSelectedId(profile.id);
      setEditorProfile(profile);
      setNotice({ tone: "success", text: "已新建中转站，请填写连接信息" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const saveProfile = useMutation({
    mutationFn: (values: { profileId: string; draft: ProfileDraft }) =>
      api.updateProfile(values.profileId, values.draft),
    onSuccess: async (profile) => {
      await refresh();
      await refreshModelCache(profile.id);
      setSelectedId(profile.id);
      setProfileDirty(false);
      setEditorProfile(undefined);
      setNotice({ tone: "success", text: "中转站已保存" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const duplicateProfile = useMutation({
    mutationFn: api.duplicateProfile,
    onSuccess: async (profile) => {
      await refresh();
      setSelectedId(profile.id);
      setNotice({ tone: "success", text: "中转站已复制" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const deleteProfile = useMutation({
    mutationFn: api.deleteProfile,
    onSuccess: async () => {
      await refresh();
      setConfirmAction(null);
      setNotice({ tone: "success", text: "中转站已删除，当前 Codex 配置未改动" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const importProfiles = useMutation({
    mutationFn: api.importProfiles,
    onSuccess: async (result) => {
      await refresh();
      const last = result.profiles.at(-1);
      if (last) setSelectedId(last.id);
      setNotice({ tone: "success", text: "中转站已导入；未包含密钥的中转站需补填 API Key" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const importCurrent = useMutation({
    mutationFn: api.importCurrent,
    onSuccess: async (profile) => {
      await refresh();
      setSelectedId(profile.id);
      setNotice({ tone: "success", text: "已导入当前 Codex 配置" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const exportProfiles = useMutation({
    mutationFn: api.exportProfiles,
    onSuccess: () => {
      setConfirmAction(null);
      setNotice({ tone: "success", text: "中转站已导出" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const prepareApply = useMutation({
    mutationFn: api.prepareApply,
    onSuccess: (response) => void handleActionResponse(response),
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const continueAction = useMutation({
    mutationFn: (values: { token: string; choice: string }) => api.continueApply(values.token, values.choice),
    onSuccess: (response) => {
      setConfirmation(undefined);
      void handleActionResponse(response);
    },
    onError: (error) => { setCloseAfterContextSave(false); setNotice({ tone: "error", text: messageFor(error) }); },
  });
  const saveContext = useMutation({
    mutationFn: (draft: ContextDraft) => api.saveContext(selectedProfile!.id, draft),
    onSuccess: (response) => {
      if (response.kind === "requires_confirmation") setContextDirty(false);
      void handleActionResponse(response);
    },
    onError: (error) => { setCloseAfterContextSave(false); setDeferredSelectionId(undefined); setNotice({ tone: "error", text: messageFor(error) }); },
  });
  const prepareRestore = useMutation({
    mutationFn: api.prepareRestore,
    onSuccess: (response) => {
      setConfirmAction(null);
      void handleActionResponse(response);
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const exportUsage = useMutation({
    mutationFn: () => api.exportUsage(selectedProfile!.id, usagePeriod),
    onSuccess: () => setNotice({ tone: "success", text: "用量明细已导出" }),
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });

  function finishWindowCloseAfterContextSave() {
    if (!closeAfterContextSave) return;
    setCloseAfterContextSave(false);
    closeWindow();
  }

  async function handleActionResponse(response: ApplyResponse) {
    if (response.kind === "requires_confirmation") {
      setConfirmation(response.confirmation);
      return;
    }
    if (response.kind === "imported_current") {
      await refresh();
      setSelectedId(response.profile.id);
      if (deferredSelectionId) {
        setSelectedId(deferredSelectionId);
        setDeferredSelectionId(undefined);
        setPage("relay");
      }
      setNotice({
        tone: response.warning ? "warning" : "success",
        text: response.warning ?? "已导入当前 Codex 配置",
      });
      finishWindowCloseAfterContextSave();
      return;
    }
    if (response.kind === "restored") {
      await refresh();
      await refreshContext();
      await refreshModelCache();
      if (response.activeProfileId) setSelectedId(response.activeProfileId);
      setNotice({
        tone: response.warning ? "warning" : "success",
        text: response.warning ?? "已恢复最近备份",
      });
      return;
    }
    if (response.kind === "context_saved") {
      setContextDraft(response.context);
      setContextDirty(false);
      await refreshContext();
      if (deferredSelectionId) {
        setSelectedId(deferredSelectionId);
        setDeferredSelectionId(undefined);
        setPage("relay");
      }
      setNotice({
        tone: response.warning ? "warning" : "success",
        text: response.warning ?? response.context.status,
      });
      finishWindowCloseAfterContextSave();
      return;
    }
    await refresh();
    await refreshContext();
    await refreshModelCache();
    setSelectedId(response.activeProfileId);
    setNotice({
      tone: response.warning ? "warning" : "success",
      text: response.warning ?? "切换完成",
    });
  }

  function selectProfile(id: string) {
    if (contextDirty) {
      setPendingSelection({ profileId: id });
      return;
    }
    setSelectedId(id);
    setContextDirty(false);
    setPage("relay");
  }

  function ensureContextSaved() {
    if (!contextDirty) return true;
    setPage("context");
    setNotice({ tone: "warning", text: "请先保存或恢复上下文配置" });
    return false;
  }

  const busy =
    newProfile.isPending ||
    saveProfile.isPending ||
    duplicateProfile.isPending ||
    deleteProfile.isPending ||
    prepareRestore.isPending ||
    importProfiles.isPending ||
    importCurrent.isPending ||
    exportProfiles.isPending ||
    prepareApply.isPending ||
    continueAction.isPending ||
    saveContext.isPending ||
    exportUsage.isPending;
  const canSaveContext = contextDirty || context.data?.syncState === "unsynced";

  useEffect(() => {
    const preventBrowserClose = (event: BeforeUnloadEvent) => {
      if (allowWindowClose.current || (!busy && !profileDirty && !contextDirty)) return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", preventBrowserClose);

    if (!isTauriRuntime()) {
      return () => window.removeEventListener("beforeunload", preventBrowserClose);
    }

    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow().onCloseRequested((event) => {
      if (allowWindowClose.current || (!busy && !profileDirty && !contextDirty)) return;
      event.preventDefault();
      if (busy) {
        setNotice({ tone: "warning", text: "操作正在进行，请等待完成后再关闭" });
      } else if (profileDirty) {
        setWindowCloseEditorRequest((request) => request + 1);
      } else {
        setWindowCloseContext(true);
      }
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });

    return () => {
      disposed = true;
      unlisten?.();
      window.removeEventListener("beforeunload", preventBrowserClose);
    };
  }, [busy, contextDirty, profileDirty]);

  return (
    <main className="legacy-app-shell">
      <aside className="legacy-sidebar" aria-label="中转站">
        <header className="legacy-sidebar-title">
          <strong>中转站</strong>
          <button
            className="legacy-icon-button on-dark"
            type="button"
            title="新建中转站"
            aria-label="新建中转站"
            disabled={busy}
            onClick={() => { if (ensureContextSaved()) newProfile.mutate(); }}
          >
            <Plus size={17} />
          </button>
        </header>
        {profiles.length === 0 ? (
          <div className="legacy-empty-sidebar">
            <strong>还没有中转站</strong>
            <span>可从当前 Codex 配置导入</span>
            <button
              className="legacy-command-button"
              type="button"
              disabled={busy}
              onClick={() => importCurrent.mutate()}
            >
              导入当前配置
            </button>
          </div>
        ) : (
          <nav className="legacy-profile-list">
            {profiles.map((profile) => (
              <button
                className={`legacy-profile-row ${selectedProfile?.id === profile.id ? "selected" : ""}`}
                type="button"
                key={profile.id}
                disabled={busy}
                onClick={() => selectProfile(profile.id)}
              >
                <span className="legacy-profile-copy">
                  <strong>{profile.name}</strong>
                  <small>{profile.model || "未设置模型"}</small>
                </span>
                <span className="legacy-active-label">{profile.isActive ? "使用中" : ""}</span>
              </button>
            ))}
          </nav>
        )}
        <footer className="legacy-sidebar-footer">
          <button type="button" disabled={busy} onClick={() => { if (ensureContextSaved()) importProfiles.mutate(); }}><Upload size={14} />导入</button>
          <button type="button" disabled={profiles.length === 0 || busy} onClick={() => { if (ensureContextSaved()) setConfirmAction("export"); }}><Download size={14} />导出</button>
          <button type="button" disabled={!bootstrap.data?.canRestore || busy} onClick={() => { if (ensureContextSaved()) setConfirmAction("restore"); }}><History size={14} />恢复</button>
          <span />
          <button type="button" title="关于 Codex Switch" aria-label="关于 Codex Switch" onClick={() => setNotice({ tone: "success", text: "Codex Switch" })}><Info size={16} /></button>
        </footer>
      </aside>

      <section className="legacy-main">
        {!selectedProfile ? (
          <div className="legacy-empty-main">
            <strong>{profiles.length === 0 ? "创建一个中转站" : "选择一个中转站"}</strong>
            <span>{profiles.length === 0 ? "填写连接信息后保存" : "查看当前连接、上下文和用量"}</span>
          </div>
        ) : (
          <>
            <header className="legacy-profile-header">
              <div>
                <strong>{selectedProfile.name || "未命名中转站"}</strong>
                <span>{selectedProfile.baseUrl || "未设置接口地址"}</span>
              </div>
              <b>{selectedProfile.isActive ? "使用中" : ""}</b>
            </header>
            <nav className="legacy-tabs" aria-label="中转站详情">
              <Tab label="中转站" active={page === "relay"} onClick={() => setPage("relay")} />
              <Tab label="上下文" active={page === "context"} onClick={() => setPage("context")} />
              <Tab label="统计" active={page === "usage"} onClick={() => setPage("usage")} />
            </nav>
            <section className="legacy-page-content">
              {page === "relay" && <RelayPage profile={selectedProfile} context={context.data} modelCache={modelCache.data} usage={todayUsage.data} usageLoading={todayUsage.isLoading} usageError={todayUsage.error} onPageChange={setPage} />}
              {page === "context" && <ContextPage context={context.data} draft={contextDraft} dirty={contextDirty} loading={context.isLoading} onChange={(next) => { setContextDraft(next); setContextDirty(true); }} />}
              {page === "usage" && <UsagePage usage={usage.data} loading={usage.isLoading} error={usage.error} period={usagePeriod} onPeriod={setUsagePeriod} />}
            </section>
            <footer className="legacy-statusbar">
              <div className={`legacy-status ${notice?.tone ?? "idle"}`} role="status">
                {busy || usage.isFetching ? <RefreshCw className="spin" size={15} /> : <span className="legacy-status-dot" />}
                <span>{notice?.text ?? (page === "usage" ? usage.error ? `本地用量读取失败：${messageFor(usage.error)}` : usage.data?.status ?? "正在读取本地用量数据" : statusText(page, contextDirty, context.data))}</span>
              </div>
              {page === "relay" && <>
                <button className="legacy-command-button" type="button" disabled={busy} onClick={() => { if (ensureContextSaved()) setEditorProfile(selectedProfile); }}>编辑配置</button>
                <button className="legacy-command-button primary" type="button" disabled={busy} onClick={() => {
                  if (contextDirty) { setPage("context"); setNotice({ tone: "warning", text: "请先保存上下文配置，再切换中转站" }); return; }
                  prepareApply.mutate(selectedProfile.id);
                }}>{selectedProfile.isActive ? "重新应用配置" : "切换到此中转站"}</button>
              </>}
              {page === "context" && <>
                <button className="legacy-command-button" type="button" disabled={busy} onClick={() => { setContextDraft(defaultContext); setContextDirty(true); setNotice({ tone: "warning", text: "上下文已恢复为默认草稿，保存后生效" }); }}>恢复默认</button>
                <button className="legacy-command-button primary" type="button" disabled={!canSaveContext || busy} onClick={() => saveContext.mutate(contextDraft)}>{context.data?.syncState === "unsynced" && !contextDirty ? "重试同步" : "保存到配置"}</button>
              </>}
              {page === "usage" && <>
                <button className="legacy-command-button" type="button" disabled={!usage.data?.hasData || busy} onClick={() => exportUsage.mutate()}>导出 CSV</button>
                <button className="legacy-command-button primary" type="button" disabled={usage.isFetching || busy} onClick={() => usage.refetch()}>刷新数据</button>
              </>}
            </footer>
          </>
        )}
      </section>

      {selectedProfile && <ProfileTools profile={selectedProfile} busy={busy} onEdit={() => { if (ensureContextSaved()) setEditorProfile(selectedProfile); }} onDuplicate={() => { if (ensureContextSaved()) duplicateProfile.mutate(selectedProfile.id); }} onDelete={() => { if (ensureContextSaved()) setConfirmAction("delete"); }} />}
      <ProfileEditor profile={editorProfile} saving={saveProfile.isPending} windowCloseRequest={windowCloseEditorRequest} onDirtyChange={setProfileDirty} onClose={() => { setProfileDirty(false); setEditorProfile(undefined); }} onWindowCloseResolved={() => { setProfileDirty(false); setEditorProfile(undefined); if (contextDirty) setWindowCloseContext(true); else closeWindow(); }} onSubmit={(profileId, draft) => saveProfile.mutateAsync({ profileId, draft })} />
      <ActionConfirmation confirmation={confirmation} pending={continueAction.isPending} onClose={(token) => { setCloseAfterContextSave(false); setConfirmation(undefined); setDeferredSelectionId(undefined); void api.dismissConfirmation(token); void refreshContext(); }} onChoice={(token, choice) => continueAction.mutate({ token, choice })} />
      <LegacyConfirm action={confirmAction} pending={deleteProfile.isPending || prepareRestore.isPending || exportProfiles.isPending} onClose={() => setConfirmAction(null)} onConfirm={() => {
        if (confirmAction === "delete" && selectedProfile) deleteProfile.mutate(selectedProfile.id);
        if (confirmAction === "restore") prepareRestore.mutate();
        if (confirmAction === "export") exportProfiles.mutate(false);
      }} onExportWithKeys={() => exportProfiles.mutate(true)} />
      <DirtySelectionConfirmation pending={pendingSelection} saving={saveContext.isPending} onClose={() => setPendingSelection(null)} onDiscard={() => { if (!pendingSelection) return; setSelectedId(pendingSelection.profileId); setPendingSelection(null); setContextDirty(false); setPage("relay"); }} onSave={() => { if (!pendingSelection) return; setDeferredSelectionId(pendingSelection.profileId); setPendingSelection(null); saveContext.mutate(contextDraft); }} />
      <WindowCloseConfirmation open={windowCloseContext} saving={saveContext.isPending} onCancel={() => setWindowCloseContext(false)} onDiscard={() => { setWindowCloseContext(false); setContextDirty(false); closeWindow(); }} onSave={() => { setWindowCloseContext(false); setCloseAfterContextSave(true); saveContext.mutate(contextDraft); }} />
    </main>
  );
}

function Tab({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return <button className={`legacy-tab ${active ? "active" : ""}`} type="button" onClick={onClick}>{label}</button>;
}

function RelayPage({ profile, context, modelCache, usage, usageLoading, usageError, onPageChange }: { profile: ProfileSummary; context?: ContextView; modelCache?: ModelListView; usage?: UsageView; usageLoading: boolean; usageError: unknown; onPageChange: (page: Page) => void }) {
  const todayUsage = usageLoading
    ? "正在读取本地用量数据"
    : usageError
      ? `本地用量读取失败：${messageFor(usageError)}`
      : usage?.todaySummary ?? "今日暂无本地记录";
  return <div className="legacy-scroll-page">
    <Section label="连接" />
    <Summary label="接口地址" value={profile.baseUrl || "未设置"} />
    <Summary label="API Key" value={profile.hasApiKey ? "已配置" : "未配置"} />
    <div className="legacy-spacer" />
    <Section label="模型" />
    <Summary label="默认模型" value={profile.model || "未设置"} />
    <Summary label="审查模型" value={profile.reviewModel || "跟随默认模型"} />
    <Summary label="模型列表" value={modelCache?.cacheLabel ?? "尚未获取模型列表"} />
    <div className="legacy-entry-grid">
      <button type="button" onClick={() => onPageChange("context")}><span>上下文</span><strong>{context?.summary ?? "自动窗口 · 输出不限 · 自动压缩"}</strong><em>在「上下文」标签页调整 →</em></button>
      <button type="button" onClick={() => onPageChange("usage")}><span>今日用量</span><strong>{todayUsage}</strong><em>查看统计明细 →</em></button>
    </div>
  </div>;
}

function ContextPage({ context, draft, dirty, loading, onChange }: { context?: ContextView; draft: ContextDraft; dirty: boolean; loading: boolean; onChange: (next: ContextDraft) => void }) {
  const budget = context?.budget;
  return <div className="legacy-scroll-page">
    <Section label="窗口与输出" />
    <label className="legacy-context-row"><span>上下文窗口</span><div><input aria-label="上下文窗口（K）" value={draft.windowK} disabled={draft.useDefaults || loading} placeholder="自动" inputMode="decimal" onChange={(event) => onChange({ ...draft, useDefaults: false, windowK: event.target.value })} /><b>K</b></div></label>
    <label className="legacy-context-row"><span>自动压缩阈值</span><div className="legacy-percent-control"><input aria-label="自动压缩阈值" type="range" min="50" max="95" value={draft.compactPercent} disabled={draft.useDefaults || loading} onChange={(event) => onChange({ ...draft, useDefaults: false, compactPercent: Number(event.target.value) })} /><b>{draft.compactPercent}%</b></div></label>
    <div className="legacy-default-row"><label className="legacy-default-toggle"><input type="checkbox" checked={draft.useDefaults} disabled={loading} onChange={(event) => onChange(event.target.checked ? { ...defaultContext } : { ...draft, useDefaults: false, windowK: draft.windowK || context?.budget.suggestedWindowK || "272" })} /><span><Check size={14} /></span></label><div><strong>使用 Codex 默认上下文</strong><small>自动选择窗口，并沿用 Codex 的自动压缩行为。</small></div><button type="button" onClick={() => onChange({ ...defaultContext })} disabled={loading}>恢复默认</button></div>
    <div className="legacy-spacer" />
    <Section label="上下文预算 · 最近会话估算" />
    <div className="legacy-budget-grid"><Metric label="最近会话" value={budget?.recentSession ?? "暂无记录"} /><Metric label="指令文件" value={budget?.instructionTokens ?? "暂无记录"} /><Metric label="可用预算" value={budget?.availableBudget ?? "自动"} /></div>
    <div className="legacy-budget-bar" aria-label="上下文预算分布"><span className="history" style={{ width: `${(budget?.historyRatio ?? 0) * 100}%` }} /><span className="instructions" style={{ width: `${(budget?.instructionRatio ?? 0) * 100}%` }} /><span className="remaining" style={{ width: `${(budget?.remainingRatio ?? 1) * 100}%` }} /></div>
    {budget?.instructions.length ? <div className="legacy-instruction-list">{budget.instructions.map((source) => <div key={`${source.name}-${source.detail}`}><strong>{source.name}</strong><span>{source.detail}</span></div>)}</div> : null}
    {dirty && <p className="legacy-inline-warning">上下文配置 · 有未保存修改</p>}
  </div>;
}

function UsagePage({ usage, loading, error, period, onPeriod }: { usage?: UsageView; loading: boolean; error: unknown; period: UsageView["period"]; onPeriod: (period: UsageView["period"]) => void }) {
  const [hoveredIndex, setHoveredIndex] = useState<number>();
  const current = usage?.current ?? { input: "0", cached: "0", output: "0", calls: "0 次" };
  const total = usage?.periodTotal ?? current;
  const label = period === "today" ? "今天" : period === "last_7_days" ? "近 7 天" : "近 30 天";
  const selected = hoveredIndex === undefined ? undefined : usage?.trend[hoveredIndex];
  return <div className="legacy-scroll-page">
    <div className="legacy-period-tabs"><button className={period === "today" ? "active" : ""} type="button" onClick={() => onPeriod("today")}>今天</button><button className={period === "last_7_days" ? "active" : ""} type="button" onClick={() => onPeriod("last_7_days")}>近 7 天</button><button className={period === "last_30_days" ? "active" : ""} type="button" onClick={() => onPeriod("last_30_days")}>近 30 天</button></div>
    <div className="legacy-usage-metrics"><Metric label="输入" value={current.input} /><Metric label="缓存" value={current.cached} /><Metric label="输出" value={current.output} /><Metric label="调用" value={current.calls} /></div>
    <Section label={`${label}用量趋势 · 蓝=输入 / 深灰=输出`} />
    {loading ? <div className="legacy-chart-empty">正在读取本地用量数据</div> : error ? <div className="legacy-chart-empty">本地用量读取失败：{messageFor(error)}</div> : usage?.trend.length ? <UsageChart trend={usage.trend} onHover={setHoveredIndex} /> : <div className="legacy-chart-empty">暂无本地用量数据</div>}
    {selected && <div className="legacy-usage-hover"><strong>{selected.label}</strong><span>输入 {selected.usage.input} · 缓存 {selected.usage.cached} · 输出 {selected.usage.output} · 调用 {selected.usage.calls}</span></div>}
    {period !== "today" && <><div className="legacy-spacer" /><Section label={`${label}总计`} /><div className="legacy-usage-metrics"><Metric label="输入" value={total.input} /><Metric label="缓存" value={total.cached} /><Metric label="输出" value={total.output} /><Metric label="调用" value={total.calls} /></div></>}
    <Section label={`${label}模型分布`} />
    <div className="legacy-usage-table"><div><span>模型</span><span>输入</span><span>缓存</span><span>输出</span><span>调用</span></div>{error ? <p>读取失败：{messageFor(error)}</p> : usage?.models.length ? usage.models.map((model) => <div className="legacy-usage-row" key={model.model}><strong>{model.model}</strong><span>{model.input}</span><span>{model.cached}</span><span>{model.output}</span><span>{model.calls}</span></div>) : <p>暂无模型明细</p>}</div>
  </div>;
}

function UsageChart({ trend, onHover }: { trend: UsageView["trend"]; onHover: (index: number | undefined) => void }) {
  const points = (key: "inputRatio" | "outputRatio") => trend.map((point, index) => {
    const x = trend.length === 1 ? 50 : (index / (trend.length - 1)) * 100;
    const y = 94 - point[key] * 84;
    return `${x},${y}`;
  }).join(" ");
  return <div className="legacy-chart" onMouseLeave={() => onHover(undefined)}>
    <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-label="用量趋势图">
      <polyline points={points("inputRatio")} className="input-line" vectorEffect="non-scaling-stroke" />
      <polyline points={points("outputRatio")} className="output-line" vectorEffect="non-scaling-stroke" />
    </svg>
    <div className="legacy-chart-hit-targets" style={{ gridTemplateColumns: `repeat(${trend.length}, minmax(0, 1fr))` }}>{trend.map((point, index) => <button type="button" aria-label={`${point.label} 用量`} key={point.label} onMouseEnter={() => onHover(index)} onFocus={() => onHover(index)} />)}</div>
  </div>;
}

function Section({ label }: { label: string }) { return <><h2 className="legacy-section-label">{label}</h2><div className="legacy-rule" /></>; }
function Summary({ label, value }: { label: string; value: string }) { return <div className="legacy-summary-row"><span>{label}</span><strong>{value}</strong></div>; }
function Metric({ label, value }: { label: string; value: string }) { return <div className="legacy-metric"><span>{label}</span><strong>{value}</strong></div>; }

function ProfileTools({ profile, busy, onEdit, onDuplicate, onDelete }: { profile: ProfileSummary; busy: boolean; onEdit: () => void; onDuplicate: () => void; onDelete: () => void }) {
  return <div className="legacy-profile-tools"><button type="button" title="编辑中转站" aria-label="编辑中转站" onClick={onEdit}><Pencil size={16} /></button><button type="button" title="复制此中转站" aria-label="复制此中转站" disabled={busy} onClick={onDuplicate}><Copy size={16} /></button><button type="button" title="删除此中转站" aria-label="删除此中转站" disabled={busy} onClick={onDelete}><Trash2 size={16} /></button><span>{profile.name}</span></div>;
}

function ProfileEditor({ profile, saving, windowCloseRequest, onDirtyChange, onClose, onWindowCloseResolved, onSubmit }: { profile?: ProfileSummary; saving: boolean; windowCloseRequest: number; onDirtyChange: (dirty: boolean) => void; onClose: () => void; onWindowCloseResolved: () => void; onSubmit: (profileId: string, draft: ProfileDraft) => Promise<unknown> }) {
  const form = useForm<ProfileFormValues>({ resolver: zodResolver(profileSchema), defaultValues: emptyProfileForm });
  const [reviewEnabled, setReviewEnabled] = useState(false);
  const [closeIntent, setCloseIntent] = useState<"editor" | "window" | null>(null);
  const lastWindowCloseRequest = useRef(0);
  const cachedModels = useQuery({ queryKey: ["model-cache", profile?.id], queryFn: () => api.loadModelCache(profile!.id), enabled: Boolean(profile) });
  const [models, setModels] = useState<string[]>([]);
  const [cacheLabel, setCacheLabel] = useState("尚未获取模型列表");
  const refreshModels = useMutation({
    mutationFn: () => {
      const values = form.getValues();
      return api.refreshModels(profile!.id, { ...toProfileDraft(values), clearApiKey: false });
    },
    onSuccess: (result) => { setModels(result.models); setCacheLabel(result.cacheLabel); },
  });

  useEffect(() => {
    if (!profile) return;
    form.reset({ name: profile.name, baseUrl: profile.baseUrl, apiKey: "", clearApiKey: false, model: profile.model, reviewModel: profile.reviewModel ?? "" });
    setReviewEnabled(Boolean(profile.reviewModel));
    setCloseIntent(null);
  }, [form, profile?.id]);
  useEffect(() => {
    if (cachedModels.data) { setModels(cachedModels.data.models); setCacheLabel(cachedModels.data.cacheLabel); }
  }, [cachedModels.data]);

  const dirty = Boolean(profile) && (form.formState.isDirty || reviewEnabled !== Boolean(profile?.reviewModel));
  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => {
    if (!profile || windowCloseRequest === 0 || lastWindowCloseRequest.current === windowCloseRequest) return;
    lastWindowCloseRequest.current = windowCloseRequest;
    if (dirty) setCloseIntent("window");
    else onWindowCloseResolved();
  }, [dirty, onWindowCloseResolved, profile, windowCloseRequest]);

  const requestClose = (intent: "editor" | "window") => {
    if (dirty) {
      setCloseIntent(intent);
      return;
    }
    if (intent === "window") onWindowCloseResolved();
    else onClose();
  };
  const submit = (after?: () => void) => {
    void form.handleSubmit(async (values) => {
      if (!profile) return;
      try {
        await onSubmit(profile.id, { ...toProfileDraft(values), reviewModel: reviewEnabled ? values.reviewModel?.trim() || undefined : undefined });
        after?.();
      } catch {
        // The mutation has already placed its user-facing error in the status bar.
      }
    })();
  };
  const discard = () => {
    const intent = closeIntent;
    setCloseIntent(null);
    onDirtyChange(false);
    if (intent === "window") onWindowCloseResolved();
    else onClose();
  };
  const saveAndClose = () => {
    const intent = closeIntent;
    submit(() => {
      setCloseIntent(null);
      if (intent === "window") onWindowCloseResolved();
    });
  };

  return <><Dialog.Root open={Boolean(profile)} onOpenChange={(open) => !open && profile && requestClose("editor")}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-editor-dialog" aria-describedby={undefined}>
    <header><Dialog.Title>编辑中转站</Dialog.Title><button type="button" title="关闭" aria-label="关闭" onClick={() => requestClose("editor")}><X size={17} /></button></header>
    <form onSubmit={(event) => { event.preventDefault(); submit(); }}>
      <Field label="名称" error={form.formState.errors.name?.message}><input autoFocus {...form.register("name")} placeholder="我的中转站" /></Field>
      <Field label="接口地址" error={form.formState.errors.baseUrl?.message}><input {...form.register("baseUrl")} placeholder="https://relay.example.com/v1" /></Field>
      <Field label="API Key" error={form.formState.errors.apiKey?.message}><input type="password" autoComplete="off" {...form.register("apiKey")} placeholder="留空以保留已保存的凭据" /><span className="legacy-clear-key"><input type="checkbox" {...form.register("clearApiKey")} />清除已保存的 API Key</span></Field>
      <Field label="默认模型" error={form.formState.errors.model?.message}>
        <div className="legacy-model-field"><input list="codex-switch-models" {...form.register("model")} placeholder="gpt-5.2-codex" /><button className="legacy-icon-button" type="button" title="刷新模型列表" aria-label="刷新模型列表" disabled={refreshModels.isPending || !profile} onClick={() => refreshModels.mutate()}><RefreshCw className={refreshModels.isPending ? "spin" : ""} size={16} /></button></div>
        <datalist id="codex-switch-models">{models.map((model) => <option value={model} key={model} />)}</datalist>
        <small className="legacy-cache-label">{refreshModels.error ? messageFor(refreshModels.error) : cacheLabel}</small>
      </Field>
      <label className="legacy-review-toggle"><input type="checkbox" checked={reviewEnabled} onChange={(event) => setReviewEnabled(event.target.checked)} />单独设置审查模型</label>
      {reviewEnabled && <Field label="审查模型" error={form.formState.errors.reviewModel?.message}><input {...form.register("reviewModel")} placeholder="跟随默认模型" /></Field>}
      <footer><button className="legacy-command-button" type="button" onClick={() => requestClose("editor")}>取消</button><button className="legacy-command-button primary" type="submit" disabled={saving}><Save size={15} />{saving ? "正在保存" : "保存"}</button></footer>
    </form>
  </Dialog.Content></Dialog.Portal></Dialog.Root>
  <Dialog.Root open={Boolean(closeIntent)} onOpenChange={(open) => !open && setCloseIntent(null)}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{closeIntent === "window" ? "关闭 Codex Switch" : "放弃未保存配置"}</Dialog.Title><Dialog.Description>当前中转站有未保存的配置修改。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={discard}>{closeIntent === "window" ? "放弃并关闭" : "放弃"}</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={saveAndClose}>{saving ? "正在保存" : closeIntent === "window" ? "保存并关闭" : "保存"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root></>;
}

function Field({ label, error, children }: { label: string; error?: string; children: React.ReactNode }) { return <label className="legacy-field"><span>{label}</span>{children}{error && <em>{error}</em>}</label>; }

function ActionConfirmation({ confirmation, pending, onClose, onChoice }: { confirmation?: Confirmation; pending: boolean; onClose: (token: string) => void; onChoice: (token: string, choice: string) => void }) {
  return <Dialog.Root open={Boolean(confirmation)} onOpenChange={(open) => !open && confirmation && onClose(confirmation.token)}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{confirmation?.title}</Dialog.Title><Dialog.Description>{confirmation?.detail}</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close>{confirmation?.options.map((option) => <button key={option.id} className={confirmationClass(option.intent)} type="button" disabled={pending} onClick={() => onChoice(confirmation.token, option.id)}>{pending ? "正在处理" : option.label}</button>)}</footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function LegacyConfirm({ action, pending, onClose, onConfirm, onExportWithKeys }: { action: ConfirmAction; pending: boolean; onClose: () => void; onConfirm: () => void; onExportWithKeys: () => void }) {
  const copy = action === "delete" ? ["删除中转站", "只会删除工具保存的中转站，不会修改当前 Codex 配置。", "删除"] : action === "restore" ? ["恢复最近备份", "恢复会同时还原 config.toml 和 auth.json，并先为当前文件再创建一份备份。", "继续恢复"] : ["导出中转站", "默认导出不包含 API Key。", "导出（不含 Key）"];
  return <Dialog.Root open={Boolean(action)} onOpenChange={(open) => !open && onClose()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{copy[0]}</Dialog.Title><Dialog.Description>{copy[1]}</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close>{action === "export" && <button className="legacy-command-button" type="button" disabled={pending} onClick={onExportWithKeys}>包含 Key 导出</button>}<button className={action === "delete" ? "legacy-command-button danger" : "legacy-command-button primary"} type="button" disabled={pending} onClick={onConfirm}>{pending ? "正在处理" : copy[2]}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function DirtySelectionConfirmation({ pending, saving, onClose, onDiscard, onSave }: { pending: PendingSelection; saving: boolean; onClose: () => void; onDiscard: () => void; onSave: () => void }) {
  return <Dialog.Root open={Boolean(pending)} onOpenChange={(open) => !open && onClose()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>切换中转站</Dialog.Title><Dialog.Description>当前中转站有未保存的上下文配置。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={onDiscard}>放弃并切换</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={onSave}>{saving ? "正在保存" : "保存并切换"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function WindowCloseConfirmation({ open, saving, onCancel, onDiscard, onSave }: { open: boolean; saving: boolean; onCancel: () => void; onDiscard: () => void; onSave: () => void }) {
  return <Dialog.Root open={open} onOpenChange={(next) => !next && onCancel()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>关闭 Codex Switch</Dialog.Title><Dialog.Description>当前中转站有未保存的上下文配置。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={onDiscard}>放弃并关闭</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={onSave}>{saving ? "正在保存" : "保存并关闭"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function confirmationClass(intent: ConfirmationIntent) { return intent === "danger" ? "legacy-command-button danger" : "legacy-command-button primary"; }
function statusText(page: Page, dirty: boolean, context?: ContextView) { if (page === "context") return dirty ? "上下文配置 · 有未保存修改" : context?.status ?? "上下文配置 · 已保存"; if (page === "usage") return "暂无用量数据"; return "就绪"; }
function messageFor(error: unknown) { return error instanceof Error ? error.message : "操作未完成，请重新尝试"; }
