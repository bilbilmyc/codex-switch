import * as Dialog from "@radix-ui/react-dialog";
import { zodResolver } from "@hookform/resolvers/zod";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Activity,
  Check,
  ChevronRight,
  CircleCheck,
  CircleAlert,
  Copy,
  Cpu,
  Download,
  History,
  Info,
  KeyRound,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  Route,
  Save,
  Search,
  Send,
  Server,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useForm } from "react-hook-form";
import { api } from "./api";
import { BackupCenterDialog, formatBackupLocalTime } from "./BackupCenterDialog";
import {
  deepValidationActionLabel,
  deepValidationPresentation,
  type DeepValidationCheck,
} from "./deep-validation";
import {
  emptyProfileForm,
  profileSchema,
  toProfileDraft,
  type ProfileFormValues,
} from "./profile-draft";
import {
  describeApplyState,
  filterProfiles,
  profileSummaryToDraft,
  quickModelDraftState,
  routeHost,
} from "./profile-console";
import { RouteAuditDialog, type RouteAuditStatus } from "./RouteAuditDialog";
import {
  runRouteAudit,
  profileConfigurationIssue,
  summarizeRouteAudit,
  type ConnectionCheck,
  type RouteAuditEntry,
  type RouteAuditSession,
} from "./route-audit";
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
type ConfirmAction = "delete" | "export" | null;
type PendingSelection = { profileId: string } | null;
type EditorSession = { mode: "create" } | { mode: "edit"; profile: ProfileSummary };
type QuickModelDraft = { profileId: string; value: string } | null;

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
  const [editorSession, setEditorSession] = useState<EditorSession>();
  const [confirmation, setConfirmation] = useState<Confirmation>();
  const [confirmAction, setConfirmAction] = useState<ConfirmAction>(null);
  const [pendingSelection, setPendingSelection] = useState<PendingSelection>(null);
  const [deferredSelectionId, setDeferredSelectionId] = useState<string>();
  const [notice, setNotice] = useState<Notice>(null);
  const [profileQuery, setProfileQuery] = useState("");
  const [quickSwitcherOpen, setQuickSwitcherOpen] = useState(false);
  const [backupCenterOpen, setBackupCenterOpen] = useState(false);
  const [routeAuditOpen, setRouteAuditOpen] = useState(false);
  const [routeAuditStatus, setRouteAuditStatus] = useState<RouteAuditStatus>("idle");
  const [routeAuditSession, setRouteAuditSession] = useState<RouteAuditSession>();
  const [connectionChecks, setConnectionChecks] = useState<Record<string, ConnectionCheck>>({});
  const [deepValidationChecks, setDeepValidationChecks] = useState<Record<string, DeepValidationCheck>>({});
  const [quickModelDraft, setQuickModelDraft] = useState<QuickModelDraft>(null);
  const [contextDraft, setContextDraft] = useState<ContextDraft>(defaultContext);
  const [contextDirty, setContextDirty] = useState(false);
  const [profileDirty, setProfileDirty] = useState(false);
  const [windowCloseQuickModel, setWindowCloseQuickModel] = useState(false);
  const [windowCloseContext, setWindowCloseContext] = useState(false);
  const [windowCloseEditorRequest, setWindowCloseEditorRequest] = useState(0);
  const [closeAfterContextSave, setCloseAfterContextSave] = useState(false);
  const [usagePeriod, setUsagePeriod] = useState<UsageView["period"]>("today");
  const allowWindowClose = useRef(false);
  const closeAfterQuickModelSave = useRef(false);
  const routeAuditRunId = useRef(0);
  const routeAuditStopRequested = useRef(false);
  const pendingBackupRestoreAt = useRef<number | undefined>(undefined);
  const deepValidationInFlight = useRef(false);

  const profiles = bootstrap.data?.profiles ?? [];
  const selectedProfile = useMemo(
    () => profiles.find((profile) => profile.id === selectedId) ?? profiles[0],
    [profiles, selectedId],
  );
  const filteredProfiles = useMemo(
    () => filterProfiles(profiles, profileQuery),
    [profileQuery, profiles],
  );
  const quickModel = selectedProfile && quickModelDraft?.profileId === selectedProfile.id
    ? quickModelDraft.value
    : selectedProfile?.model ?? "";
  const quickModelState = quickModelDraftState(selectedProfile?.model ?? "", quickModel);
  const quickModelDirty = Boolean(
    selectedProfile
      && quickModelDraft?.profileId === selectedProfile.id
      && quickModelState.dirty,
  );
  const selectedApplyState = selectedProfile
    ? describeApplyState(selectedProfile.applyState)
    : undefined;
  const routeAuditBusy = routeAuditStatus === "running"
    || routeAuditStatus === "stopping"
    || routeAuditStatus === "retrying";
  const editorProfile = editorSession?.mode === "edit" ? editorSession.profile : undefined;
  const editorMode = editorSession?.mode;
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
  const refreshBackups = async () => {
    await queryClient.invalidateQueries({ queryKey: ["backup-center"] });
    await queryClient.invalidateQueries({ queryKey: ["backup-preview"] });
  };
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

  const createProfile = useMutation({
    mutationFn: api.createProfile,
    onSuccess: async (profile) => {
      invalidateRouteAudit();
      await refresh();
      setSelectedId(profile.id);
      setQuickModelDraft(null);
      setProfileDirty(false);
      setEditorSession(undefined);
      setNotice({ tone: "success", text: "中转站已创建" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const saveProfile = useMutation({
    mutationFn: (values: { profileId: string; draft: ProfileDraft }) =>
      api.updateProfile(values.profileId, values.draft),
    onSuccess: async (profile) => {
      invalidateRouteAudit();
      clearConnectionCheck(profile.id);
      clearDeepValidation(profile.id);
      await refresh();
      await refreshModelCache(profile.id);
      setSelectedId(profile.id);
      setQuickModelDraft(null);
      setProfileDirty(false);
      setEditorSession(undefined);
      setNotice({ tone: "success", text: "中转站已保存" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const duplicateProfile = useMutation({
    mutationFn: api.duplicateProfile,
    onSuccess: async (profile) => {
      invalidateRouteAudit();
      await refresh();
      setSelectedId(profile.id);
      setQuickModelDraft(null);
      setNotice({ tone: "success", text: "中转站已复制" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const deleteProfile = useMutation({
    mutationFn: api.deleteProfile,
    onSuccess: async (_result, profileId) => {
      invalidateRouteAudit();
      clearDeepValidation(profileId);
      await refresh();
      setQuickModelDraft(null);
      setConfirmAction(null);
      setNotice({ tone: "success", text: "中转站已删除，当前 Codex 配置未改动" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const importProfiles = useMutation({
    mutationFn: api.importProfiles,
    onSuccess: async (result) => {
      invalidateRouteAudit();
      setConnectionChecks({});
      setDeepValidationChecks({});
      await refresh();
      const last = result.profiles.at(-1);
      if (last) setSelectedId(last.id);
      setQuickModelDraft(null);
      setNotice({ tone: "success", text: "中转站已导入；未包含密钥的中转站需补填 API Key" });
    },
    onError: (error) => setNotice({ tone: "error", text: messageFor(error) }),
  });
  const importCurrent = useMutation({
    mutationFn: api.importCurrent,
    onSuccess: async (profile) => {
      invalidateRouteAudit();
      setDeepValidationChecks({});
      await refresh();
      setSelectedId(profile.id);
      setQuickModelDraft(null);
      setConnectionChecks({});
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
  const checkConnection = useMutation({
    mutationFn: async (profile: ProfileSummary) => {
      return {
        profileId: profile.id,
        connection: await probeProfileConnection(profile),
      };
    },
    onMutate: (profile) => {
      invalidateRouteAudit();
      const connection: ConnectionCheck = { state: "checking" };
      storeConnectionCheck(profile.id, connection);
    },
    onSuccess: ({ profileId, connection }) => {
      storeConnectionCheck(profileId, connection);
      setNotice({
        tone: "success",
        text: `连接正常，已获取 ${connection.models.models.length} 个模型`,
      });
    },
    onError: (error, profile) => {
      const message = messageFor(error);
      const connection: ConnectionCheck = { state: "error", checkedAt: Date.now(), message };
      storeConnectionCheck(profile.id, connection);
      setNotice({ tone: "error", text: `连接检查失败：${message}` });
    },
  });
  const deepValidateProfile = useMutation({
    mutationFn: async (profile: ProfileSummary) => ({
      profileId: profile.id,
      result: await api.deepValidateProfile(profile.id),
    }),
    onMutate: (profile) => {
      storeDeepValidation(profile.id, { state: "running" });
      setNotice({ tone: "warning", text: "正在发送真实模型验证请求；完成前无法取消" });
    },
    onSuccess: ({ profileId, result }) => {
      storeDeepValidation(profileId, { state: "result", result });
      const presentation = deepValidationPresentation({ state: "result", result });
      setNotice({
        tone: result.status === "success" ? "success" : "error",
        text: result.status === "success"
          ? `深度验证通过，真实请求耗时 ${presentation.duration}`
          : `深度验证失败：${presentation.category}`,
      });
    },
    onError: (error, profile) => {
      const message = messageFor(error);
      storeDeepValidation(profile.id, { state: "invoke_error", message, checkedAtUnixMs: Date.now() });
      setNotice({ tone: "error", text: `深度验证未完成：${message}` });
    },
    onSettled: () => {
      deepValidationInFlight.current = false;
    },
  });
  const updateQuickModel = useMutation({
    mutationFn: ({ profile, model }: { profile: ProfileSummary; model: string; applyAfter: boolean }) =>
      api.updateProfile(profile.id, { ...profileSummaryToDraft(profile), model: model.trim() }),
    onSuccess: async (profile, variables) => {
      invalidateRouteAudit();
      clearConnectionCheck(profile.id);
      clearDeepValidation(profile.id);
      await refresh();
      await refreshModelCache(profile.id);
      setSelectedId(profile.id);
      setQuickModelDraft(null);
      if (variables.applyAfter) {
        prepareApply.mutate(profile.id);
      } else {
        setNotice({ tone: "success", text: "默认模型已保存" });
      }
      if (closeAfterQuickModelSave.current) {
        closeAfterQuickModelSave.current = false;
        if (contextDirty) setWindowCloseContext(true);
        else closeWindow();
      }
    },
    onError: (error) => {
      if (closeAfterQuickModelSave.current) {
        closeAfterQuickModelSave.current = false;
        setWindowCloseQuickModel(true);
      }
      setNotice({ tone: "error", text: messageFor(error) });
    },
  });
  const continueAction = useMutation({
    mutationFn: (values: { token: string; choice: string }) => api.continueApply(values.token, values.choice),
    onSuccess: (response) => {
      setConfirmation(undefined);
      void handleActionResponse(response);
    },
    onError: (error) => {
      const backupRestoreFailed = pendingBackupRestoreAt.current !== undefined;
      setCloseAfterContextSave(false);
      setDeferredSelectionId(undefined);
      setConfirmation(undefined);
      pendingBackupRestoreAt.current = undefined;
      if (backupRestoreFailed) void refreshBackups();
      setNotice({ tone: "error", text: messageFor(error) });
    },
  });
  const saveContext = useMutation({
    mutationFn: (draft: ContextDraft) => api.saveContext(selectedProfile!.id, draft),
    onSuccess: (response) => {
      if (response.kind === "requires_confirmation") setContextDirty(false);
      void handleActionResponse(response);
    },
    onError: (error) => { setCloseAfterContextSave(false); setDeferredSelectionId(undefined); setNotice({ tone: "error", text: messageFor(error) }); },
  });
  const prepareBackupRestore = useMutation({
    mutationFn: (values: { backupId: string; liveRevision: string }) =>
      api.prepareBackupRestore(values.backupId, values.liveRevision),
    onSuccess: (response) => void handleActionResponse(response),
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
      invalidateRouteAudit();
      await refresh();
      setSelectedId(response.profile.id);
      setQuickModelDraft(null);
      setConnectionChecks({});
      setDeepValidationChecks({});
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
      const restoredAt = pendingBackupRestoreAt.current;
      pendingBackupRestoreAt.current = undefined;
      setBackupCenterOpen(false);
      invalidateRouteAudit();
      await refresh();
      await refreshContext();
      await refreshModelCache();
      await refreshBackups();
      setQuickModelDraft(null);
      setConnectionChecks({});
      if (response.activeProfileId) setSelectedId(response.activeProfileId);
      setNotice({
        tone: response.warning ? "warning" : "success",
        text: response.warning ?? (restoredAt
          ? `已恢复 ${formatBackupLocalTime(restoredAt)} 的快照；恢复前状态已保存为新的回滚点`
          : "已恢复备份；恢复前状态已保存为新的回滚点"),
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
    setQuickModelDraft(null);
    setNotice({
      tone: response.warning ? "warning" : "success",
      text: response.warning ?? "切换完成",
    });
  }

  function selectProfile(id: string) {
    if (id === selectedProfile?.id) {
      setPage("relay");
      return;
    }
    if (!ensureQuickModelSaved()) return;
    if (contextDirty) {
      setPendingSelection({ profileId: id });
      return;
    }
    setSelectedId(id);
    setQuickModelDraft(null);
    setContextDirty(false);
    setPage("relay");
  }

  function applySelectedProfile() {
    if (busy || !selectedProfile) return;
    if (quickModelDirty) {
      saveQuickModel(true);
      return;
    }
    if (!ensureContextSaved()) return;
    prepareApply.mutate(selectedProfile.id);
  }

  function saveQuickModel(applyAfter: boolean) {
    if (busy || !selectedProfile || !quickModelState.dirty) return;
    if (!quickModelState.valid) {
      setNotice({ tone: "warning", text: "默认模型不能为空，请填写模型 ID 或撤销修改" });
      return;
    }
    if (applyAfter && !ensureContextSaved()) return;
    updateQuickModel.mutate({ profile: selectedProfile, model: quickModel, applyAfter });
  }

  function changeQuickModel(value: string) {
    if (busy || !selectedProfile) return;
    clearDeepValidation(selectedProfile.id);
    setQuickModelDraft({ profileId: selectedProfile.id, value });
    setNotice({
      tone: "warning",
      text: value.trim() ? "默认模型有未保存修改" : "默认模型不能为空，请填写模型 ID 或撤销修改",
    });
  }

  function resetQuickModel() {
    if (busy) return;
    setQuickModelDraft(null);
    setNotice({ tone: "success", text: "已撤销默认模型修改" });
  }

  function changePage(next: Page) {
    if (next !== "relay" && !ensureQuickModelSaved()) return;
    setPage(next);
  }

  function openProfileEditor(mode: "create" | "edit") {
    if (busy || !ensureWorkspaceSaved()) return;
    setEditorSession(mode === "create" ? { mode } : { mode, profile: selectedProfile! });
  }

  function runConnectionCheck() {
    if (busy || !selectedProfile) return;
    if (!ensureQuickModelSaved()) return;
    checkConnection.mutate(selectedProfile);
  }

  function runDeepValidation() {
    if (deepValidationInFlight.current || busy || !selectedProfile) return;
    if (!ensureQuickModelSaved()) return;
    const issue = profileConfigurationIssue(selectedProfile);
    if (issue) {
      setNotice({ tone: "warning", text: `无法深度验证：${issue}` });
      return;
    }
    deepValidationInFlight.current = true;
    deepValidateProfile.mutate(selectedProfile);
  }

  async function probeProfileConnection(profile: ProfileSummary): Promise<Extract<ConnectionCheck, { state: "success" }>> {
    const startedAt = performance.now();
    const models = await api.refreshModels(profile.id, profileSummaryToDraft(profile));
    return {
      state: "success",
      models,
      latencyMs: Math.max(1, Math.round(performance.now() - startedAt)),
      checkedAt: Date.now(),
    };
  }

  function openRouteAudit() {
    if (routeAuditBusy) {
      setRouteAuditOpen(true);
      return;
    }
    if (!ensureWorkspaceSaved()) return;
    setRouteAuditOpen(true);
  }

  async function startRouteAudit() {
    if (busy || profiles.length === 0 || !ensureWorkspaceSaved()) return;
    const runId = routeAuditRunId.current + 1;
    routeAuditRunId.current = runId;
    routeAuditStopRequested.current = false;
    setRouteAuditStatus("running");
    setRouteAuditSession(undefined);
    setRouteAuditOpen(true);
    setConnectionChecks((checks) => {
      const next = { ...checks };
      for (const profile of profiles) delete next[profile.id];
      return next;
    });
    setNotice({ tone: "warning", text: `正在巡检 ${profiles.length} 个中转站` });

    const finished = await runRouteAudit({
      profiles: [...profiles],
      check: (profile) => api.refreshModels(profile.id, profileSummaryToDraft(profile)),
      formatError: messageFor,
      shouldStop: () => routeAuditStopRequested.current,
      isCurrent: () => routeAuditRunId.current === runId,
      onEntry: (entry, session) => {
        if (routeAuditRunId.current !== runId) return;
        setRouteAuditSession(session);
        const connection = connectionFromAuditEntry(entry);
        if (connection) storeConnectionCheck(entry.id, connection);
        else if (entry.state === "incomplete") clearConnectionCheck(entry.id);
      },
      onProgress: (session) => {
        if (routeAuditRunId.current === runId) setRouteAuditSession(session);
      },
    });

    if (routeAuditRunId.current !== runId) return;
    setRouteAuditSession(finished);
    const stopped = finished.summary.stopped > 0;
    setRouteAuditStatus(stopped ? "stopped" : "complete");
    setNotice({
      tone: finished.summary.error > 0 ? "warning" : "success",
      text: stopped
        ? `巡检已停止，${finished.summary.success + finished.summary.error + finished.summary.incomplete}/${finished.summary.total} 已完成`
        : `巡检完成：${finished.summary.success} 可用，${finished.summary.incomplete} 未配置，${finished.summary.error} 失败`,
    });
  }

  function stopRouteAudit() {
    if (routeAuditStatus !== "running") return;
    routeAuditStopRequested.current = true;
    setRouteAuditStatus("stopping");
  }

  async function retryRouteAuditProfile(profile: ProfileSummary) {
    if (busy || routeAuditBusy) return;
    const runId = routeAuditRunId.current + 1;
    routeAuditRunId.current = runId;
    const remainingStopped = routeAuditSession?.entries.some(
      (entry) => entry.id !== profile.id && entry.state === "stopped",
    ) ?? false;
    setRouteAuditStatus("retrying");
    const checking: ConnectionCheck = { state: "checking" };
    storeConnectionCheck(profile.id, checking);
    mergeAuditConnection(profile.id, checking);

    try {
      const connection = await probeProfileConnection(profile);
      if (routeAuditRunId.current !== runId) return;
      storeConnectionCheck(profile.id, connection);
      mergeAuditConnection(profile.id, connection);
      setRouteAuditStatus(remainingStopped ? "stopped" : "complete");
      setNotice({ tone: "success", text: `${profile.name} 连接正常，${connection.latencyMs} ms` });
    } catch (error) {
      if (routeAuditRunId.current !== runId) return;
      const connection: ConnectionCheck = {
        state: "error",
        message: messageFor(error),
        checkedAt: Date.now(),
      };
      storeConnectionCheck(profile.id, connection);
      mergeAuditConnection(profile.id, connection);
      setRouteAuditStatus(remainingStopped ? "stopped" : "complete");
      setNotice({ tone: "error", text: `${profile.name} 重试失败：${connection.message}` });
    }
  }

  function editRouteAuditProfile(profile: ProfileSummary) {
    if (busy || !ensureWorkspaceSaved()) return;
    setRouteAuditOpen(false);
    setSelectedId(profile.id);
    setPage("relay");
    setEditorSession({ mode: "edit", profile });
  }

  function applyRouteAuditProfile(profile: ProfileSummary) {
    if (busy || !ensureWorkspaceSaved()) return;
    setRouteAuditOpen(false);
    prepareApply.mutate(profile.id);
  }

  async function copyBaseUrl() {
    if (!selectedProfile?.baseUrl) return;
    try {
      await navigator.clipboard.writeText(selectedProfile.baseUrl);
      setNotice({ tone: "success", text: "接口地址已复制" });
    } catch {
      setNotice({ tone: "error", text: "接口地址复制失败" });
    }
  }

  function ensureContextSaved() {
    if (!contextDirty) return true;
    setPage("context");
    setNotice({ tone: "warning", text: "请先保存或恢复上下文配置" });
    return false;
  }

  function ensureQuickModelSaved() {
    if (!quickModelDirty) return true;
    setPage("relay");
    setNotice({ tone: "warning", text: "默认模型有未保存修改，请先保存或撤销" });
    requestAnimationFrame(() => {
      const input = document.getElementById("quick-model-input") as HTMLInputElement | null;
      input?.scrollIntoView({ block: "center" });
      input?.focus({ preventScroll: true });
    });
    return false;
  }

  function ensureWorkspaceSaved() {
    return ensureQuickModelSaved() && ensureContextSaved();
  }

  function clearConnectionCheck(profileId: string) {
    setConnectionChecks((checks) => {
      if (!(profileId in checks)) return checks;
      const next = { ...checks };
      delete next[profileId];
      return next;
    });
  }

  function storeConnectionCheck(profileId: string, connection: ConnectionCheck) {
    if (connection.state === "success") {
      queryClient.setQueryData(["model-cache", profileId], connection.models);
    }
    setConnectionChecks((checks) => ({ ...checks, [profileId]: connection }));
  }

  function storeDeepValidation(profileId: string, check: DeepValidationCheck) {
    setDeepValidationChecks((current) => ({ ...current, [profileId]: check }));
  }

  function clearDeepValidation(profileId: string) {
    setDeepValidationChecks((current) => {
      if (!(profileId in current)) return current;
      const next = { ...current };
      delete next[profileId];
      return next;
    });
  }

  function mergeAuditConnection(profileId: string, connection: ConnectionCheck) {
    setRouteAuditSession((current) => {
      if (!current || !current.entries.some((entry) => entry.id === profileId)) return current;
      const entries = current.entries.map<RouteAuditEntry>((entry) => (
        entry.id === profileId
          ? { id: entry.id, name: entry.name, ...connection }
          : entry
      ));
      return {
        ...current,
        finishedAt: connection.state === "checking" ? undefined : Date.now(),
        entries,
        summary: summarizeRouteAudit(entries),
      };
    });
  }

  function invalidateRouteAudit() {
    routeAuditRunId.current += 1;
    routeAuditStopRequested.current = true;
    setRouteAuditStatus("idle");
    setRouteAuditSession(undefined);
  }

  const busy =
    createProfile.isPending ||
    saveProfile.isPending ||
    duplicateProfile.isPending ||
    deleteProfile.isPending ||
    prepareBackupRestore.isPending ||
    importProfiles.isPending ||
    importCurrent.isPending ||
    exportProfiles.isPending ||
    prepareApply.isPending ||
    checkConnection.isPending ||
    deepValidateProfile.isPending ||
    routeAuditBusy ||
    updateQuickModel.isPending ||
    continueAction.isPending ||
    saveContext.isPending ||
    exportUsage.isPending;
  const canSaveContext = contextDirty || context.data?.syncState === "unsynced";
  const modalOpen = Boolean(
    editorSession
      || confirmation
      || confirmAction
      || pendingSelection
      || routeAuditOpen
      || backupCenterOpen
      || windowCloseQuickModel
      || windowCloseContext,
  );
  const requestQuickSwitcher = useCallback(() => {
    if (busy || profileDirty || modalOpen) return;
    if (!ensureQuickModelSaved()) return;
    setQuickSwitcherOpen(true);
  }, [busy, modalOpen, profileDirty, quickModelDirty]);

  useEffect(() => {
    const openQuickSwitcher = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        requestQuickSwitcher();
      }
    };
    window.addEventListener("keydown", openQuickSwitcher);
    return () => window.removeEventListener("keydown", openQuickSwitcher);
  }, [requestQuickSwitcher]);

  useEffect(() => {
    const preventBrowserClose = (event: BeforeUnloadEvent) => {
      if (allowWindowClose.current || (!busy && !profileDirty && !quickModelDirty && !contextDirty)) return;
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
      if (allowWindowClose.current || (!busy && !profileDirty && !quickModelDirty && !contextDirty)) return;
      event.preventDefault();
      if (busy) {
        setNotice({ tone: "warning", text: "操作正在进行，请等待完成后再关闭" });
      } else if (profileDirty) {
        setWindowCloseEditorRequest((request) => request + 1);
      } else if (quickModelDirty) {
        setPage("relay");
        setWindowCloseQuickModel(true);
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
  }, [busy, contextDirty, profileDirty, quickModelDirty]);

  return (
    <main className="legacy-app-shell">
      <aside className="legacy-sidebar" aria-label="中转站">
        <header className="legacy-sidebar-title">
          <strong>中转站 <span>{profiles.length}</span></strong>
          <button
            className="legacy-icon-button on-dark"
            type="button"
            title="新建中转站"
            aria-label="新建中转站"
            disabled={busy}
            onClick={() => openProfileEditor("create")}
          >
            <Plus size={17} />
          </button>
        </header>
        <div className="legacy-sidebar-search">
          <Search size={15} />
          <input
            aria-label="搜索中转站"
            placeholder="搜索名称、模型或地址"
            value={profileQuery}
            onChange={(event) => setProfileQuery(event.target.value)}
          />
          <button type="button" title="巡检全部中转站" aria-label="巡检全部中转站" disabled={profiles.length === 0 || (busy && !routeAuditBusy)} onClick={openRouteAudit}>
            {routeAuditBusy ? <RefreshCw className="spin" size={15} /> : <Activity size={15} />}
          </button>
          <button type="button" title="快速切换" aria-label="快速切换" onClick={requestQuickSwitcher}>
            <Route size={15} />
          </button>
        </div>
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
        ) : filteredProfiles.length === 0 ? (
          <div className="legacy-empty-sidebar compact">
            <strong>没有匹配项</strong>
            <span>换个名称、模型或地址试试</span>
          </div>
        ) : (
          <nav className="legacy-profile-list">
            {filteredProfiles.map((profile) => {
              const applyState = describeApplyState(profile.applyState);
              const hasModelDraft = quickModelDirty && selectedProfile?.id === profile.id;
              const connection = connectionChecks[profile.id];
              const badge = hasModelDraft
                ? { label: "未保存", tone: "warning" }
                : connection?.state === "checking"
                  ? { label: "检查中", tone: "checking" }
                  : connection?.state === "success"
                    ? { label: `${connection.latencyMs} ms`, tone: "success" }
                    : connection?.state === "error"
                      ? { label: "失败", tone: "error" }
                      : { label: applyState.badge, tone: applyState.tone };
              return <button
                className={`legacy-profile-row ${selectedProfile?.id === profile.id ? "selected" : ""}`}
                type="button"
                key={profile.id}
                disabled={busy}
                onClick={() => selectProfile(profile.id)}
              >
                <span className={`legacy-profile-health ${profileHealthClass(profile, connection)}`} />
                <span className="legacy-profile-copy">
                  <strong>{profile.name}</strong>
                  <small>{profile.model || "未设置模型"}</small>
                </span>
                <span className={`legacy-active-label ${badge.tone}`}>{badge.label}</span>
              </button>;
            })}
          </nav>
        )}
        <footer className="legacy-sidebar-footer">
          <button type="button" disabled={busy} onClick={() => { if (ensureWorkspaceSaved()) importProfiles.mutate(); }}><Upload size={14} />导入</button>
          <button type="button" disabled={profiles.length === 0 || busy} onClick={() => { if (ensureWorkspaceSaved()) setConfirmAction("export"); }}><Download size={14} />导出</button>
          <button type="button" title="打开备份中心" disabled={bootstrap.isPending || busy} onClick={() => { if (!ensureWorkspaceSaved()) return; prepareBackupRestore.reset(); setBackupCenterOpen(true); }}><History size={14} />恢复</button>
          <span />
          <button type="button" title="关于 Codex Switch" aria-label="关于 Codex Switch" onClick={() => setNotice({ tone: "success", text: "Codex Switch" })}><Info size={16} /></button>
        </footer>
      </aside>

      <section className="legacy-main">
        {!selectedProfile ? (
          <div className="legacy-empty-main">
            <strong>{profiles.length === 0 ? "创建一个中转站" : "选择一个中转站"}</strong>
            <span>{profiles.length === 0 ? "填写连接信息后保存" : "查看当前连接、上下文和用量"}</span>
            {profiles.length === 0 && <button className="legacy-command-button primary" type="button" onClick={() => openProfileEditor("create")}><Plus size={15} />新建中转站</button>}
          </div>
        ) : (
          <>
            <header className="legacy-profile-header">
              <div className="legacy-profile-heading">
                <span className={`legacy-profile-kicker ${quickModelDirty ? "warning" : selectedApplyState!.tone}`}>{quickModelDirty ? "默认模型有未保存修改" : selectedApplyState!.kicker}</span>
                <strong>{selectedProfile.name || "未命名中转站"}</strong>
                <span>{selectedProfile.baseUrl || "未设置接口地址"}</span>
              </div>
              <div className="legacy-header-actions">
                <ProfileTools profile={selectedProfile} busy={busy} onEdit={() => openProfileEditor("edit")} onDuplicate={() => { if (ensureWorkspaceSaved()) duplicateProfile.mutate(selectedProfile.id); }} onDelete={() => { if (ensureWorkspaceSaved()) setConfirmAction("delete"); }} />
                <button className="legacy-command-button primary legacy-apply-button" type="button" disabled={busy} onClick={applySelectedProfile}><Route size={15} />{quickModelDirty ? "保存并应用" : selectedApplyState!.action}</button>
              </div>
            </header>
            <RouteBand profile={selectedProfile} context={context.data} connection={connectionChecks[selectedProfile.id]} />
            <nav className="legacy-tabs" aria-label="中转站详情">
              <Tab label="概览" active={page === "relay"} onClick={() => changePage("relay")} />
              <Tab label="上下文" active={page === "context"} onClick={() => changePage("context")} />
              <Tab label="用量" active={page === "usage"} onClick={() => changePage("usage")} />
            </nav>
            <section className="legacy-page-content">
              {page === "relay" && <RelayPage profile={selectedProfile} context={context.data} modelCache={modelCache.data} usage={todayUsage.data} usageLoading={todayUsage.isLoading} usageError={todayUsage.error} connection={connectionChecks[selectedProfile.id]} deepValidation={deepValidationChecks[selectedProfile.id]} modelDraft={quickModel} modelDirty={quickModelDirty} modelSaving={updateQuickModel.isPending} locked={busy} onModelDraft={changeQuickModel} onResetModel={resetQuickModel} onSaveModel={saveQuickModel} onCheck={runConnectionCheck} onDeepValidate={runDeepValidation} onCopyUrl={() => void copyBaseUrl()} onEdit={() => openProfileEditor("edit")} onPageChange={changePage} />}
              {page === "context" && <ContextPage context={context.data} draft={contextDraft} dirty={contextDirty} loading={context.isLoading} onChange={(next) => { setContextDraft(next); setContextDirty(true); }} />}
              {page === "usage" && <UsagePage usage={usage.data} loading={usage.isLoading} error={usage.error} period={usagePeriod} onPeriod={setUsagePeriod} />}
            </section>
            <footer className="legacy-statusbar">
              <div className={`legacy-status ${notice?.tone ?? "idle"}`} role="status">
                {busy || usage.isFetching || checkConnection.isPending ? <RefreshCw className="spin" size={15} /> : <span className="legacy-status-dot" />}
                <span>{notice?.text ?? (page === "usage" ? usage.error ? `本地用量读取失败：${messageFor(usage.error)}` : usage.data?.status ?? "正在读取本地用量数据" : statusText(page, contextDirty, quickModelDirty, context.data))}</span>
              </div>
              {page === "relay" && <>
                <button className="legacy-command-button" type="button" title={quickModelDirty ? "请先保存或撤销默认模型修改" : undefined} disabled={busy || quickModelDirty} onClick={runConnectionCheck}><Activity size={15} />{checkConnection.isPending ? "正在检查" : "检查连接"}</button>
                <button className="legacy-command-button" type="button" disabled={busy} onClick={() => openProfileEditor("edit")}><Pencil size={15} />编辑配置</button>
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

      <ProfileEditor mode={editorMode} profile={editorProfile} saving={saveProfile.isPending || createProfile.isPending} windowCloseRequest={windowCloseEditorRequest} onDirtyChange={setProfileDirty} onClose={() => { setProfileDirty(false); setEditorSession(undefined); }} onWindowCloseResolved={() => { setProfileDirty(false); setEditorSession(undefined); if (contextDirty) setWindowCloseContext(true); else closeWindow(); }} onSubmit={(profileId, draft) => profileId ? saveProfile.mutateAsync({ profileId, draft }) : createProfile.mutateAsync(draft)} />
      <RouteAuditDialog open={routeAuditOpen} profiles={profiles} session={routeAuditSession} status={routeAuditStatus} locked={busy} onOpenChange={setRouteAuditOpen} actions={{ start: () => void startRouteAudit(), stop: stopRouteAudit, retry: (profile) => void retryRouteAuditProfile(profile), edit: editRouteAuditProfile, apply: applyRouteAuditProfile }} />
      <BackupCenterDialog open={backupCenterOpen} locked={busy} restorePending={prepareBackupRestore.isPending} restoreError={prepareBackupRestore.error ? messageFor(prepareBackupRestore.error) : undefined} onOpenChange={(open) => { setBackupCenterOpen(open); if (!open) { pendingBackupRestoreAt.current = undefined; prepareBackupRestore.reset(); } }} onClearRestoreError={() => prepareBackupRestore.reset()} onRestore={(backupId, liveRevision, createdAtUnixMs) => { pendingBackupRestoreAt.current = createdAtUnixMs; prepareBackupRestore.mutate({ backupId, liveRevision }); }} />
      <QuickSwitcher open={quickSwitcherOpen} profiles={profiles} selectedId={selectedProfile?.id} onOpenChange={setQuickSwitcherOpen} onSelect={(profileId) => { setQuickSwitcherOpen(false); selectProfile(profileId); }} onCreate={() => { setQuickSwitcherOpen(false); openProfileEditor("create"); }} />
      <ActionConfirmation confirmation={confirmation} pending={continueAction.isPending} onClose={(token) => { setCloseAfterContextSave(false); setConfirmation(undefined); setDeferredSelectionId(undefined); pendingBackupRestoreAt.current = undefined; void api.dismissConfirmation(token); void refreshContext(); }} onChoice={(token, choice) => continueAction.mutate({ token, choice })} />
      <LegacyConfirm action={confirmAction} pending={deleteProfile.isPending || exportProfiles.isPending} onClose={() => setConfirmAction(null)} onConfirm={() => {
        if (confirmAction === "delete" && selectedProfile) deleteProfile.mutate(selectedProfile.id);
        if (confirmAction === "export") exportProfiles.mutate(false);
      }} onExportWithKeys={() => exportProfiles.mutate(true)} />
      <DirtySelectionConfirmation pending={pendingSelection} saving={saveContext.isPending} onClose={() => setPendingSelection(null)} onDiscard={() => { if (!pendingSelection) return; setSelectedId(pendingSelection.profileId); setPendingSelection(null); setContextDirty(false); setPage("relay"); }} onSave={() => { if (!pendingSelection) return; setDeferredSelectionId(pendingSelection.profileId); setPendingSelection(null); saveContext.mutate(contextDraft); }} />
      <QuickModelCloseConfirmation open={windowCloseQuickModel} saving={updateQuickModel.isPending} canSave={quickModelState.valid} onCancel={() => setWindowCloseQuickModel(false)} onDiscard={() => { setWindowCloseQuickModel(false); setQuickModelDraft(null); if (contextDirty) setWindowCloseContext(true); else closeWindow(); }} onSave={() => { setWindowCloseQuickModel(false); closeAfterQuickModelSave.current = true; saveQuickModel(false); }} />
      <WindowCloseConfirmation open={windowCloseContext} saving={saveContext.isPending} onCancel={() => setWindowCloseContext(false)} onDiscard={() => { setWindowCloseContext(false); setContextDirty(false); closeWindow(); }} onSave={() => { setWindowCloseContext(false); setCloseAfterContextSave(true); saveContext.mutate(contextDraft); }} />
    </main>
  );
}

function Tab({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return <button className={`legacy-tab ${active ? "active" : ""}`} type="button" onClick={onClick}>{label}</button>;
}

function RouteBand({ profile, context, connection }: { profile: ProfileSummary; context?: ContextView; connection?: ConnectionCheck }) {
  const host = routeHost(profile.baseUrl);
  const status = routeStatus(profile, context, connection);
  const applyState = describeApplyState(profile.applyState);
  return <div className={`legacy-route-band ${applyState.routeClass}`} aria-label="当前 Codex 路由">
    <div className="legacy-route-node source"><span><Activity size={13} />Codex</span><strong>Desktop / CLI</strong></div>
    <ChevronRight className="legacy-route-arrow" size={17} />
    <div className="legacy-route-node"><span><Server size={13} />中转地址</span><strong>{host}</strong></div>
    <ChevronRight className="legacy-route-arrow" size={17} />
    <div className="legacy-route-node"><span><Cpu size={13} />默认模型</span><strong>{profile.model || "尚未设置"}</strong></div>
    <div className={`legacy-route-state ${status.tone}`}><i /><div><strong>{status.label}</strong><small>{status.detail}</small></div></div>
  </div>;
}

function RelayPage({ profile, context, modelCache, usage, usageLoading, usageError, connection, deepValidation, modelDraft, modelDirty, modelSaving, locked, onModelDraft, onResetModel, onSaveModel, onCheck, onDeepValidate, onCopyUrl, onEdit, onPageChange }: { profile: ProfileSummary; context?: ContextView; modelCache?: ModelListView; usage?: UsageView; usageLoading: boolean; usageError: unknown; connection?: ConnectionCheck; deepValidation?: DeepValidationCheck; modelDraft: string; modelDirty: boolean; modelSaving: boolean; locked: boolean; onModelDraft: (model: string) => void; onResetModel: () => void; onSaveModel: (applyAfter: boolean) => void; onCheck: () => void; onDeepValidate: () => void; onCopyUrl: () => void; onEdit: () => void; onPageChange: (page: Page) => void }) {
  const todayUsage = usageLoading
    ? "正在读取本地用量数据"
    : usageError
      ? `本地用量读取失败：${messageFor(usageError)}`
      : usage?.todaySummary ?? "今日暂无本地记录";
  const modelState = quickModelDraftState(profile.model, modelDraft);
  const modelChanged = modelState.dirty && modelState.valid;
  const applyState = describeApplyState(profile.applyState);
  const contextReady = Boolean(context && (!context.isActive || context.syncState !== "unsynced"));
  const readiness = [
    { label: "接口地址", detail: routeHost(profile.baseUrl), ready: Boolean(profile.baseUrl), warning: false },
    { label: "API Key", detail: profile.hasApiKey ? "凭据已保存" : "需要配置凭据", ready: profile.hasApiKey, warning: false },
    { label: "默认模型", detail: modelDirty ? `${modelState.normalized || "空值"} · 未保存` : profile.model || "需要选择模型", ready: Boolean(profile.model) && !modelDirty, warning: modelDirty },
    { label: "应用状态", detail: modelDirty ? "默认模型草稿尚未保存" : applyState.readinessDetail, ready: applyState.ready && !modelDirty, warning: applyState.tone !== "error" },
    { label: "模型目录", detail: modelCache?.cacheLabel ?? "尚未检查连接", ready: connection?.state === "success", warning: true },
    { label: "上下文", detail: context?.status ?? "等待读取配置", ready: contextReady, warning: !context },
  ];
  const blocked = readiness.some((item) => !item.ready && !item.warning);
  const needsAttention = readiness.some((item) => !item.ready) || connection?.state === "error";
  const healthLabel = blocked ? "配置未就绪" : modelDirty ? "默认模型待保存" : !applyState.ready ? applyState.routeLabel : needsAttention ? "建议检查" : "可以应用";
  const healthTone = blocked || connection?.state === "error" || applyState.tone === "error" ? "error" : needsAttention ? "warning" : "success";

  return <div className="legacy-scroll-page legacy-console-overview">
    <div className="legacy-overview-grid">
      <section className="legacy-overview-primary">
        <header className="legacy-console-section-head">
          <div><span>连接与模型</span><strong>确认中转地址，并选择 Codex 要使用的模型。</strong></div>
          <button className="legacy-command-button" type="button" title={modelDirty ? "请先保存或撤销默认模型修改" : undefined} disabled={locked || connection?.state === "checking" || modelDirty} onClick={onCheck}><Activity size={15} />{connection?.state === "checking" ? "正在检查" : "检查连接"}</button>
        </header>
        <div className="legacy-detail-list">
          <div className="legacy-detail-row">
            <span>接口地址</span>
            <div className="legacy-detail-value"><strong className="legacy-mono">{profile.baseUrl || "未设置"}</strong><button className="legacy-inline-icon" type="button" title="复制接口地址" aria-label="复制接口地址" disabled={!profile.baseUrl} onClick={onCopyUrl}><Copy size={15} /></button></div>
          </div>
          <div className="legacy-detail-row">
            <span>API Key</span>
            <div className="legacy-detail-value"><strong>{profile.hasApiKey ? "已安全保存" : "尚未配置"}</strong><KeyRound size={15} /></div>
          </div>
          <div className="legacy-detail-row model">
            <span>默认模型</span>
            <div className="legacy-model-console">
              <div><div className="legacy-model-input-row"><input id="quick-model-input" aria-label="默认模型" list={`models-${profile.id}`} value={modelDraft} disabled={locked} onChange={(event) => onModelDraft(event.target.value)} placeholder="输入模型 ID" /><button className="legacy-inline-icon" type="button" title="撤销默认模型修改" aria-label="撤销默认模型修改" disabled={locked || !modelDirty || modelSaving} onClick={onResetModel}><RotateCcw size={15} /></button></div><datalist id={`models-${profile.id}`}>{modelCache?.models.map((model) => <option value={model} key={model} />)}</datalist><small>{modelDirty ? "有未保存修改，可保存或撤销" : modelCache?.cacheLabel ?? "检查连接后可从模型目录选择"}</small></div>
              <button className="legacy-command-button" type="button" disabled={locked || !modelChanged || modelSaving} onClick={() => onSaveModel(false)}><Save size={15} />保存</button>
              <button className="legacy-command-button primary" type="button" disabled={locked || !modelChanged || modelSaving} onClick={() => onSaveModel(true)}><Route size={15} />保存并应用</button>
            </div>
          </div>
          <div className="legacy-detail-row">
            <span>审查模型</span>
            <div className="legacy-detail-value"><strong className="legacy-mono">{profile.reviewModel || "跟随默认模型"}</strong></div>
          </div>
        </div>
      </section>
      <aside className="legacy-readiness-panel" aria-label="中转站就绪检查">
        <header><div><span>就绪检查</span><strong className={healthTone}>{healthLabel}</strong></div><button className="legacy-inline-link" type="button" disabled={locked} onClick={onEdit}>编辑配置</button></header>
        <div className="legacy-readiness-list">
          {readiness.map((item) => <ReadinessItem key={item.label} {...item} />)}
        </div>
        <ConnectionResult connection={connection} />
        <DeepValidationPanel profile={profile} check={deepValidation} modelDirty={modelDirty} locked={locked} onValidate={onDeepValidate} />
      </aside>
    </div>
    <div className="legacy-console-previews">
      <button type="button" aria-label={`打开上下文设置，${context?.summary ?? "使用 Codex 默认上下文"}`} onClick={() => onPageChange("context")}>
        <span><Cpu size={16} />上下文</span>
        <strong>{context?.summary ?? "自动窗口 · 输出不限 · 自动压缩"}</strong>
        <div className="legacy-mini-budget"><i style={{ width: `${(context?.budget.historyRatio ?? 0) * 100}%` }} /><i style={{ width: `${(context?.budget.instructionRatio ?? 0) * 100}%` }} /><i style={{ width: `${(context?.budget.remainingRatio ?? 1) * 100}%` }} /></div>
        <ChevronRight size={17} />
      </button>
      <button type="button" aria-label={`打开用量统计，${todayUsage}`} onClick={() => onPageChange("usage")}>
        <span><Activity size={16} />今日用量</span>
        <strong>{todayUsage}</strong>
        <small>{usage?.hasData ? `输入 ${usage.current.input} · 输出 ${usage.current.output} · ${usage.current.calls}` : "本地会话产生用量后会在这里汇总"}</small>
        <ChevronRight size={17} />
      </button>
    </div>
  </div>;
}

function ReadinessItem({ label, detail, ready, warning }: { label: string; detail: string; ready: boolean; warning: boolean }) {
  const tone = ready ? "success" : warning ? "warning" : "error";
  return <div className={`legacy-readiness-item ${tone}`}>{ready ? <CircleCheck size={16} /> : <CircleAlert size={16} />}<div><strong>{label}</strong><span>{detail}</span></div></div>;
}

function ConnectionResult({ connection }: { connection?: ConnectionCheck }) {
  if (!connection) return <div className="legacy-connection-result idle"><span>连接尚未检查</span><small>检查会请求中转站的模型列表。</small></div>;
  if (connection.state === "checking") return <div className="legacy-connection-result checking"><RefreshCw className="spin" size={15} /><div><span>正在连接中转站</span><small>读取模型目录并验证已保存凭据。</small></div></div>;
  if (connection.state === "error") return <div className="legacy-connection-result error"><CircleAlert size={15} /><div><span>连接检查失败</span><small>{connection.message}</small></div></div>;
  return <div className="legacy-connection-result success"><CircleCheck size={15} /><div><span>连接正常 · {connection.latencyMs} ms</span><small>{connection.models.models.length} 个模型 · {formatCheckTime(connection.checkedAt)}</small></div></div>;
}

function DeepValidationPanel({ profile, check, modelDirty, locked, onValidate }: { profile: ProfileSummary; check?: DeepValidationCheck; modelDirty: boolean; locked: boolean; onValidate: () => void }) {
  const issue = profileConfigurationIssue(profile);
  const running = check?.state === "running";
  const presentation = check ? deepValidationPresentation(check) : undefined;
  const disabledReason = running
    ? "真实模型请求进行中，完成前无法取消"
    : modelDirty
      ? "请先保存或撤销默认模型修改"
      : issue;
  return <section className="legacy-deep-validation" aria-label="深度验证">
    <header>
      <div><strong>深度验证</strong><span>向已保存的默认模型发送一次最小真实请求，可能消耗少量额度并产生费用。不应用配置，输出正文不会显示或保存。</span></div>
      <button className="legacy-command-button" type="button" title={disabledReason} disabled={locked || modelDirty || Boolean(issue)} onClick={onValidate}><Send size={14} />{deepValidationActionLabel(check)}</button>
    </header>
    {presentation ? <div className={`legacy-deep-validation-result ${presentation.tone}`} role={presentation.tone === "error" ? "alert" : "status"} aria-live="polite">
      {presentation.tone === "checking" ? <RefreshCw className="spin" size={16} /> : presentation.tone === "success" ? <CircleCheck size={16} /> : <CircleAlert size={16} />}
      <div><strong>{presentation.title}</strong><span>安全类别 · {presentation.category}</span>{presentation.detail ? <small>{presentation.detail}</small> : null}{presentation.usage ? <small>{presentation.usage} · 输出正文不会显示或保存</small> : null}</div>
      {presentation.duration || presentation.checkedAt ? <div className="legacy-deep-validation-metrics">{presentation.duration ? <strong><span>真实请求耗时</span>{presentation.duration}</strong> : null}{presentation.checkedAt ? <time>{presentation.checkedAt}</time> : null}</div> : null}
    </div> : null}
  </section>;
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
  return <div className="legacy-profile-tools" role="group" aria-label={`${profile.name} 操作`}><button type="button" title="编辑中转站" aria-label="编辑中转站" disabled={busy} onClick={onEdit}><Pencil size={16} /></button><button type="button" title="复制此中转站" aria-label="复制此中转站" disabled={busy} onClick={onDuplicate}><Copy size={16} /></button><button type="button" title="删除此中转站" aria-label="删除此中转站" disabled={busy} onClick={onDelete}><Trash2 size={16} /></button></div>;
}

function QuickSwitcher({ open, profiles, selectedId, onOpenChange, onSelect, onCreate }: { open: boolean; profiles: ProfileSummary[]; selectedId?: string; onOpenChange: (open: boolean) => void; onSelect: (profileId: string) => void; onCreate: () => void }) {
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const matches = useMemo(() => filterProfiles(profiles, query), [profiles, query]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    const selectedIndex = profiles.findIndex((profile) => profile.id === selectedId);
    setActiveIndex(Math.max(0, selectedIndex));
  }, [open, profiles, selectedId]);
  useEffect(() => {
    if (activeIndex >= matches.length) setActiveIndex(Math.max(0, matches.length - 1));
  }, [activeIndex, matches.length]);

  const chooseActive = () => {
    const profile = matches[activeIndex];
    if (profile) onSelect(profile.id);
  };

  return <Dialog.Root open={open} onOpenChange={onOpenChange}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-quick-switcher" aria-describedby={undefined}>
    <header><Search size={17} /><Dialog.Title>快速切换中转站</Dialog.Title><button type="button" title="关闭" aria-label="关闭" onClick={() => onOpenChange(false)}><X size={16} /></button></header>
    <div className="legacy-quick-search"><Search size={15} /><input autoFocus aria-label="搜索中转站" placeholder="输入名称、模型或地址" value={query} onChange={(event) => { setQuery(event.target.value); setActiveIndex(0); }} onKeyDown={(event) => {
      if (event.key === "ArrowDown" && matches.length) { event.preventDefault(); setActiveIndex((index) => Math.min(matches.length - 1, index + 1)); }
      if (event.key === "ArrowUp" && matches.length) { event.preventDefault(); setActiveIndex((index) => Math.max(0, index - 1)); }
      if (event.key === "Enter") { event.preventDefault(); chooseActive(); }
    }} /></div>
    <nav aria-label="匹配的中转站">{matches.length ? matches.map((profile, index) => {
      const applyState = describeApplyState(profile.applyState);
      return <button className={index === activeIndex ? "active" : ""} type="button" key={profile.id} onMouseEnter={() => setActiveIndex(index)} onClick={() => onSelect(profile.id)}><span className={`legacy-profile-health ${profileHealthClass(profile)}`} /><div><strong>{profile.name}</strong><small>{profile.model} · {routeHost(profile.baseUrl)}</small></div>{applyState.badge ? <em className={applyState.tone}>{applyState.badge}</em> : null}</button>;
    }) : <p>没有匹配的中转站</p>}</nav>
    <footer><button className="legacy-command-button" type="button" onClick={onCreate}><Plus size={15} />新建中转站</button></footer>
  </Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function ProfileEditor({ mode, profile, saving, windowCloseRequest, onDirtyChange, onClose, onWindowCloseResolved, onSubmit }: { mode?: "create" | "edit"; profile?: ProfileSummary; saving: boolean; windowCloseRequest: number; onDirtyChange: (dirty: boolean) => void; onClose: () => void; onWindowCloseResolved: () => void; onSubmit: (profileId: string | undefined, draft: ProfileDraft) => Promise<unknown> }) {
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
    if (!mode) return;
    form.reset(profile ? { name: profile.name, baseUrl: profile.baseUrl, apiKey: "", clearApiKey: false, model: profile.model, reviewModel: profile.reviewModel ?? "" } : emptyProfileForm);
    setReviewEnabled(Boolean(profile?.reviewModel));
    setCloseIntent(null);
    setModels([]);
    setCacheLabel(profile ? "尚未获取模型列表" : "创建后可检查连接并获取模型列表");
  }, [form, mode, profile?.id]);
  useEffect(() => {
    if (profile && cachedModels.data) { setModels(cachedModels.data.models); setCacheLabel(cachedModels.data.cacheLabel); }
  }, [cachedModels.data, profile]);

  const dirty = Boolean(mode) && (form.formState.isDirty || reviewEnabled !== Boolean(profile?.reviewModel));
  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => {
    if (!mode || windowCloseRequest === 0 || lastWindowCloseRequest.current === windowCloseRequest) return;
    lastWindowCloseRequest.current = windowCloseRequest;
    if (dirty) setCloseIntent("window");
    else onWindowCloseResolved();
  }, [dirty, mode, onWindowCloseResolved, windowCloseRequest]);

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
      if (!mode) return;
      try {
        await onSubmit(profile?.id, { ...toProfileDraft(values), reviewModel: reviewEnabled ? values.reviewModel?.trim() || undefined : undefined });
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

  return <><Dialog.Root open={Boolean(mode)} onOpenChange={(open) => !open && mode && requestClose("editor")}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-editor-dialog" aria-describedby={undefined}>
    <header><Dialog.Title>{mode === "create" ? "新建中转站" : "编辑中转站"}</Dialog.Title><button type="button" title="关闭" aria-label="关闭" onClick={() => requestClose("editor")}><X size={17} /></button></header>
    <form onSubmit={(event) => { event.preventDefault(); submit(); }}>
      <Field label="名称" error={form.formState.errors.name?.message}><input autoFocus {...form.register("name")} placeholder="我的中转站" /></Field>
      <Field label="接口地址" error={form.formState.errors.baseUrl?.message}><input {...form.register("baseUrl")} placeholder="https://relay.example.com/v1" /></Field>
      <Field label="API Key" error={form.formState.errors.apiKey?.message}><input type="password" autoComplete="off" {...form.register("apiKey")} placeholder={profile?.hasApiKey ? "留空以保留已保存的凭据" : "输入中转站凭据"} />{profile?.hasApiKey ? <span className="legacy-clear-key"><input type="checkbox" {...form.register("clearApiKey")} />清除已保存的 API Key</span> : null}</Field>
      <Field label="默认模型" error={form.formState.errors.model?.message}>
        <div className="legacy-model-field"><input list="codex-switch-models" {...form.register("model")} placeholder="gpt-5.2-codex" /><button className="legacy-icon-button" type="button" title="刷新模型列表" aria-label="刷新模型列表" disabled={refreshModels.isPending || !profile} onClick={() => refreshModels.mutate()}><RefreshCw className={refreshModels.isPending ? "spin" : ""} size={16} /></button></div>
        <datalist id="codex-switch-models">{models.map((model) => <option value={model} key={model} />)}</datalist>
        <small className="legacy-cache-label">{refreshModels.error ? messageFor(refreshModels.error) : cacheLabel}</small>
      </Field>
      <label className="legacy-review-toggle"><input type="checkbox" checked={reviewEnabled} onChange={(event) => setReviewEnabled(event.target.checked)} />单独设置审查模型</label>
      {reviewEnabled && <Field label="审查模型" error={form.formState.errors.reviewModel?.message}><input {...form.register("reviewModel")} placeholder="跟随默认模型" /></Field>}
      <footer><button className="legacy-command-button" type="button" onClick={() => requestClose("editor")}>取消</button><button className="legacy-command-button primary" type="submit" disabled={saving}><Save size={15} />{saving ? "正在保存" : mode === "create" ? "创建中转站" : "保存"}</button></footer>
    </form>
  </Dialog.Content></Dialog.Portal></Dialog.Root>
  <Dialog.Root open={Boolean(closeIntent)} onOpenChange={(open) => !open && setCloseIntent(null)}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{closeIntent === "window" ? "关闭 Codex Switch" : "放弃未保存配置"}</Dialog.Title><Dialog.Description>当前中转站有未保存的配置修改。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={discard}>{closeIntent === "window" ? "放弃并关闭" : "放弃"}</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={saveAndClose}>{saving ? "正在保存" : closeIntent === "window" ? "保存并关闭" : "保存"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root></>;
}

function Field({ label, error, children }: { label: string; error?: string; children: React.ReactNode }) { return <label className="legacy-field"><span>{label}</span>{children}{error && <em>{error}</em>}</label>; }

function ActionConfirmation({ confirmation, pending, onClose, onChoice }: { confirmation?: Confirmation; pending: boolean; onClose: (token: string) => void; onChoice: (token: string, choice: string) => void }) {
  return <Dialog.Root open={Boolean(confirmation)} onOpenChange={(open) => !open && confirmation && onClose(confirmation.token)}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{confirmation?.title}</Dialog.Title><Dialog.Description>{confirmation?.detail}</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close>{confirmation?.options.map((option) => <button key={option.id} className={confirmationClass(option.intent)} type="button" disabled={pending} onClick={() => onChoice(confirmation.token, option.id)}>{pending ? "正在处理" : option.label}</button>)}</footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function LegacyConfirm({ action, pending, onClose, onConfirm, onExportWithKeys }: { action: ConfirmAction; pending: boolean; onClose: () => void; onConfirm: () => void; onExportWithKeys: () => void }) {
  const copy = action === "delete" ? ["删除中转站", "只会删除工具保存的中转站，不会修改当前 Codex 配置。", "删除"] : ["导出中转站", "默认导出不包含 API Key。", "导出（不含 Key）"];
  return <Dialog.Root open={Boolean(action)} onOpenChange={(open) => !open && onClose()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>{copy[0]}</Dialog.Title><Dialog.Description>{copy[1]}</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close>{action === "export" && <button className="legacy-command-button" type="button" disabled={pending} onClick={onExportWithKeys}>包含 Key 导出</button>}<button className={action === "delete" ? "legacy-command-button danger" : "legacy-command-button primary"} type="button" disabled={pending} onClick={onConfirm}>{pending ? "正在处理" : copy[2]}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function DirtySelectionConfirmation({ pending, saving, onClose, onDiscard, onSave }: { pending: PendingSelection; saving: boolean; onClose: () => void; onDiscard: () => void; onSave: () => void }) {
  return <Dialog.Root open={Boolean(pending)} onOpenChange={(open) => !open && onClose()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>切换中转站</Dialog.Title><Dialog.Description>当前中转站有未保存的上下文配置。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={onDiscard}>放弃并切换</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={onSave}>{saving ? "正在保存" : "保存并切换"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function QuickModelCloseConfirmation({ open, saving, canSave, onCancel, onDiscard, onSave }: { open: boolean; saving: boolean; canSave: boolean; onCancel: () => void; onDiscard: () => void; onSave: () => void }) {
  return <Dialog.Root open={open} onOpenChange={(next) => !next && onCancel()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>关闭 Codex Switch</Dialog.Title><Dialog.Description>默认模型有未保存修改。可以先保存，或放弃修改后关闭。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={onDiscard}>放弃并关闭</button><button className="legacy-command-button primary" type="button" disabled={saving || !canSave} onClick={onSave}>{saving ? "正在保存" : "保存并关闭"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function WindowCloseConfirmation({ open, saving, onCancel, onDiscard, onSave }: { open: boolean; saving: boolean; onCancel: () => void; onDiscard: () => void; onSave: () => void }) {
  return <Dialog.Root open={open} onOpenChange={(next) => !next && onCancel()}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-confirm-dialog"><CircleAlert size={21} /><Dialog.Title>关闭 Codex Switch</Dialog.Title><Dialog.Description>当前中转站有未保存的上下文配置。</Dialog.Description><footer><Dialog.Close asChild><button className="legacy-command-button" type="button">取消</button></Dialog.Close><button className="legacy-command-button" type="button" disabled={saving} onClick={onDiscard}>放弃并关闭</button><button className="legacy-command-button primary" type="button" disabled={saving} onClick={onSave}>{saving ? "正在保存" : "保存并关闭"}</button></footer></Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function confirmationClass(intent: ConfirmationIntent) { return intent === "danger" ? "legacy-command-button danger" : "legacy-command-button primary"; }
function statusText(page: Page, contextDirty: boolean, quickModelDirty: boolean, context?: ContextView) { if (page === "context") return contextDirty ? "上下文配置 · 有未保存修改" : context?.status ?? "上下文配置 · 已保存"; if (page === "usage") return "暂无用量数据"; return quickModelDirty ? "默认模型有未保存修改" : "就绪"; }
function connectionFromAuditEntry(entry: RouteAuditEntry): ConnectionCheck | undefined {
  if (entry.state === "checking") return { state: "checking" };
  if (entry.state === "success") return { state: "success", models: entry.models, latencyMs: entry.latencyMs, checkedAt: entry.checkedAt };
  if (entry.state === "error") return { state: "error", message: entry.message, checkedAt: entry.checkedAt };
  return undefined;
}
function routeStatus(profile: ProfileSummary, context?: ContextView, connection?: ConnectionCheck) {
  if (connection?.state === "checking") return { tone: "checking", label: "正在检查", detail: "读取模型目录" };
  if (connection?.state === "error") return { tone: "error", label: "连接异常", detail: connection.message ?? "检查中转站配置" };
  if (!profile.baseUrl || !profile.hasApiKey || !profile.model) return { tone: "warning", label: "配置未就绪", detail: "补全地址、凭据和模型" };
  const applyState = describeApplyState(profile.applyState);
  if (profile.applyState === "applied" && context?.syncState === "unsynced") return { tone: "warning", label: "上下文待同步", detail: "保存上下文配置后生效" };
  if (profile.applyState === "applied" && connection?.state === "success") return { tone: "success", label: applyState.routeLabel, detail: `连接 ${connection.latencyMs} ms` };
  return { tone: applyState.tone, label: applyState.routeLabel, detail: applyState.routeDetail };
}
function profileHealthClass(profile: ProfileSummary, connection?: ConnectionCheck) {
  if (connection?.state === "error") return "error";
  if (connection?.state === "checking") return "checking";
  if (!profile.baseUrl || !profile.hasApiKey || !profile.model) return "warning";
  const applyState = describeApplyState(profile.applyState);
  if (applyState.healthClass !== "idle") return applyState.healthClass;
  if (connection?.state === "success") return "success";
  return "idle";
}
function formatCheckTime(checkedAt?: number) {
  if (!checkedAt) return "刚刚";
  return new Date(checkedAt).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}
function messageFor(error: unknown) {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error) return error.message;
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") return error.message;
  return "操作未完成，请重新尝试";
}
