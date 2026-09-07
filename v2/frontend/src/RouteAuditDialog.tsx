import * as Dialog from "@radix-ui/react-dialog";
import {
  Activity,
  Circle,
  CircleAlert,
  CircleCheck,
  Pencil,
  RefreshCw,
  Route,
  Square,
  X,
} from "lucide-react";
import { useMemo } from "react";
import { describeApplyState, routeHost } from "./profile-console";
import {
  canApplyRouteAuditEntry,
  profileConfigurationIssue,
  routeAuditHistoryRefreshCount,
  routeAuditStaleReasonLabel,
  summarizeRouteAudit,
  type RouteAuditEntry,
  type RouteAuditSession,
} from "./route-audit";
import type { ProfileSummary } from "./types";

export type RouteAuditStatus = "idle" | "loading" | "history_error" | "history_warning" | "history" | "stale" | "running" | "stopping" | "retrying" | "complete" | "stopped";

type RouteAuditActions = {
  start: () => void;
  stop: () => void;
  retry: (profile: ProfileSummary) => void;
  edit: (profile: ProfileSummary) => void;
  apply: (profile: ProfileSummary) => void;
  reloadHistory: () => void;
};

type RouteAuditDialogProps = {
  open: boolean;
  profiles: ProfileSummary[];
  session?: RouteAuditSession;
  status: RouteAuditStatus;
  locked: boolean;
  historyMessage?: string;
  onOpenChange: (open: boolean) => void;
  actions: RouteAuditActions;
};

export function RouteAuditDialog({ open, profiles, session, status, locked, historyMessage, onOpenChange, actions }: RouteAuditDialogProps) {
  const profileById = useMemo(
    () => new Map(profiles.map((profile) => [profile.id, profile])),
    [profiles],
  );
  const entries = useMemo(() => {
    if (session) return session.entries;
    return profiles.map<RouteAuditEntry>((profile) => {
      const issue = profileConfigurationIssue(profile);
      return issue
        ? { id: profile.id, name: profile.name, state: "incomplete", issue }
        : { id: profile.id, name: profile.name, state: "queued" };
    });
  }, [profileById, profiles, session]);
  const summary = useMemo(() => summarizeRouteAudit(entries), [entries]);
  const resolved = summary.success + summary.error + summary.incomplete;
  const running = status === "running" || status === "stopping" || status === "retrying";
  const headline = auditHeadline(status, session, summary.total, resolved, summary.success, summary.error, summary.incomplete);
  const footer = auditFooter(status, session, summary);

  return <Dialog.Root open={open} onOpenChange={onOpenChange}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-route-audit" aria-describedby={undefined}>
    <header className="legacy-audit-header">
      <Activity size={19} />
      <div><Dialog.Title>全路由巡检</Dialog.Title><span aria-live="polite">{headline}</span></div>
      <button type="button" title="关闭巡检" aria-label="关闭巡检" onClick={() => onOpenChange(false)}><X size={17} /></button>
    </header>
    <div className="legacy-audit-list">
      {status === "loading" ? <div className="legacy-audit-empty"><RefreshCw className="spin" size={19} /><strong>正在读取上次巡检结果</strong><span>只读取脱敏摘要，不会加载模型列表或凭据。</span></div>
        : status === "history_error" ? <div className="legacy-audit-empty error"><CircleAlert size={19} /><strong>上次巡检结果读取失败</strong><span>{historyMessage ?? "可以重试读取，或直接开始新的巡检。"}</span><button className="legacy-command-button" type="button" onClick={actions.reloadHistory}><RefreshCw size={14} />重新读取</button></div>
          : status === "history_warning" ? <div className="legacy-audit-empty warning"><CircleAlert size={19} /><strong>上次巡检结果未载入</strong><span>{historyMessage ?? "历史摘要不可用，可以直接开始新的巡检。"}</span><button className="legacy-command-button" type="button" onClick={actions.reloadHistory}><RefreshCw size={14} />重新读取</button></div>
          : entries.length ? entries.map((entry) => {
        const profile = profileById.get(entry.id);
        return <AuditRow key={entry.id} entry={entry} profile={profile} historicalSession={session?.source === "history"} locked={locked || running} actions={actions} />;
      }) : <div className="legacy-audit-empty"><strong>还没有中转站</strong><span>创建中转站后即可一次检查全部路由。</span></div>}
    </div>
    <footer className="legacy-audit-footer">
      <div><strong>{footer.title}</strong><span>{footer.detail}</span></div>
      <button className="legacy-command-button" type="button" onClick={() => onOpenChange(false)}>关闭</button>
      {running
        ? status === "retrying"
          ? <button className="legacy-command-button" type="button" disabled><RefreshCw className="spin" size={14} />正在重试</button>
          : <button className="legacy-command-button" type="button" disabled={status === "stopping"} onClick={actions.stop}><Square size={14} />{status === "stopping" ? "正在停止" : "停止"}</button>
        : <button className="legacy-command-button primary" type="button" disabled={locked || profiles.length === 0} onClick={actions.start}><Activity size={15} />{status === "idle" || status === "history_error" || status === "history_warning" ? "开始巡检" : "重新巡检"}</button>}
    </footer>
  </Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function AuditRow({ entry, profile, historicalSession, locked, actions }: { entry: RouteAuditEntry; profile?: ProfileSummary; historicalSession: boolean; locked: boolean; actions: RouteAuditActions }) {
  const profileName = profile?.name ?? entry.name;
  const presentation = auditEntryPresentation(entry, historicalSession);
  const applyState = profile ? describeApplyState(profile.applyState) : undefined;
  const canRetry = Boolean(profile) && (entry.state === "success" || entry.state === "error" || entry.state === "stopped");
  const retryVisible = canRetry || entry.state === "checking";
  const retryLocked = locked || entry.state === "checking";
  const canApply = Boolean(profile) && canApplyRouteAuditEntry(entry);

  return <div className={`legacy-audit-row ${presentation.tone}`} aria-label={`${profileName}，${presentation.label}`}>
    <div className="legacy-audit-row-icon">{presentation.icon}</div>
    <div className="legacy-audit-profile">
      <strong>{profileName}</strong>
      <span>{profile ? `${routeHost(profile.baseUrl)} · ${profile.model || "未设置模型"}` : "历史中转站 · 当前已删除"}</span>
    </div>
    <div className="legacy-audit-result">
      <strong>{presentation.label}</strong>
      <span>{presentation.detail}</span>
    </div>
    <div className="legacy-audit-actions" role="group" aria-label={`${profileName} 巡检操作`}>
      {retryVisible && profile ? <button type="button" title={entry.state === "checking" ? `正在检查 ${profile.name}` : `重新检查 ${profile.name}`} aria-label={entry.state === "checking" ? `正在检查 ${profile.name}` : `重新检查 ${profile.name}`} aria-disabled={retryLocked} onClick={() => { if (!retryLocked) actions.retry(profile); }}><RefreshCw className={entry.state === "checking" ? "spin" : undefined} size={15} /></button> : null}
      {profile ? <button type="button" title={`编辑 ${profile.name}`} aria-label={`编辑 ${profile.name}`} disabled={locked} onClick={() => actions.edit(profile)}><Pencil size={15} /></button> : null}
      {canApply && profile && applyState ? <button className="apply" type="button" title={`${applyState.action}：${profile.name}`} aria-label={`${applyState.action}：${profile.name}`} disabled={locked} onClick={() => actions.apply(profile)}><Route size={15} /></button> : null}
    </div>
  </div>;
}

function auditEntryPresentation(entry: RouteAuditEntry, historicalSession: boolean) {
  if (entry.history?.stale) {
    const reasons = entry.history.staleReasons.map((reason) => routeAuditStaleReasonLabel(reason, entry.history!.staleAfterMs)).join("、");
    const previous = entry.state === "success" ? "上次可用" : entry.state === "error" ? "上次失败" : entry.state === "incomplete" ? "上次未配置" : "上次未检查";
    const checkedAt = "checkedAt" in entry && entry.checkedAt ? ` · ${formatAuditTime(entry.checkedAt)}` : "";
    return { tone: "stale", label: "已过期", detail: `${reasons || "结果不再适用"} · ${previous}${checkedAt}`, icon: <CircleAlert size={17} /> };
  }
  switch (entry.state) {
    case "checking":
      return { tone: "checking", label: "正在检查", detail: "正在读取模型目录", icon: <RefreshCw className="spin" size={17} /> };
    case "success":
      return { tone: "success", label: entry.history ? "上次可用" : "连接可用", detail: `${entry.latencyMs} ms · ${entry.modelCount ?? entry.models?.models.length ?? 0} 个模型 · ${formatAuditTime(entry.checkedAt)}`, icon: <CircleCheck size={17} /> };
    case "error":
      return { tone: "error", label: entry.history ? "上次失败" : "连接失败", detail: `${entry.message}${entry.checkedAt ? ` · ${formatAuditTime(entry.checkedAt)}` : ""}`, icon: <CircleAlert size={17} /> };
    case "incomplete":
      return { tone: "incomplete", label: entry.history ? "上次未配置" : "配置未完整", detail: entry.checkedAt ? `${entry.issue} · ${formatAuditTime(entry.checkedAt)}` : entry.issue, icon: <CircleAlert size={17} /> };
    case "stopped":
      return { tone: "stopped", label: entry.history ? "上次未检查" : "本轮未检查", detail: entry.checkedAt ? `巡检已停止 · ${formatAuditTime(entry.checkedAt)}` : "巡检已停止", icon: <Square size={14} /> };
    case "queued":
      return historicalSession
        ? { tone: "queued", label: "上次未包含", detail: "重新巡检后更新", icon: <Circle size={14} /> }
        : { tone: "queued", label: "等待检查", detail: "将在前一项完成后开始", icon: <Circle size={14} /> };
  }
}

function auditHeadline(status: RouteAuditStatus, session: RouteAuditSession | undefined, total: number, resolved: number, success: number, error: number, incomplete: number) {
  if (status === "loading") return "正在读取上次巡检结果";
  if (status === "history_error") return "上次巡检结果读取失败";
  if (status === "history_warning") return "上次巡检结果未载入";
  if (status === "history") return `上次巡检 · ${session?.finishedAt ? formatAuditTime(session.finishedAt) : "时间未知"}`;
  if (status === "stale") return `上次结果需更新 · ${routeAuditHistoryRefreshCount(session)}/${total} 项需重检`;
  if (status === "idle") return `${total} 个中转站 · 尚未巡检`;
  if (status === "running") return `正在检查 ${Math.min(resolved + 1, total)}/${total}`;
  if (status === "stopping") return `完成当前检查后停止 · ${resolved}/${total}`;
  if (status === "retrying") return `正在重试 · ${resolved}/${total} 已完成`;
  const prefix = status === "stopped" ? "巡检已停止" : "巡检完成";
  return `${prefix} · ${success} 可用 / ${incomplete} 未配置 / ${error} 失败`;
}

function auditFooter(status: RouteAuditStatus, session: RouteAuditSession | undefined, summary: ReturnType<typeof summarizeRouteAudit>) {
  if (status === "loading") return { title: "读取脱敏摘要", detail: "不会读取或保存模型列表、URL 或凭据。" };
  if (status === "history_error") return { title: "历史结果暂不可用", detail: "可重新读取，或直接开始新的巡检。" };
  if (status === "history_warning") return { title: "历史摘要已安全忽略", detail: "可以直接开始新的巡检。" };
  if (status === "history") return { title: `上次巡检 · ${session?.finishedAt ? formatAuditTime(session.finishedAt) : "时间未知"}`, detail: `${summary.success} 可用 / ${summary.incomplete} 未配置 / ${summary.error} 失败` };
  if (status === "stale") return { title: `${routeAuditHistoryRefreshCount(session)} 项结果需更新`, detail: `配置已变、新增中转站或结果超过 ${Math.max(1, Math.round((session?.staleAfterMs ?? 86_400_000) / 3_600_000))} 小时，请重新巡检。` };
  if (status === "idle") return { title: "检查已保存的连接", detail: "巡检不会自动切换或修改 Codex 配置。" };
  if (status === "running" || status === "stopping" || status === "retrying") return { title: `${summary.success + summary.error + summary.incomplete}/${summary.total} 已完成`, detail: "结果会逐项写回侧栏状态。" };
  if (summary.fastest) return { title: `本次最快 · ${summary.fastest.name}`, detail: `${summary.fastest.latencyMs} ms，仅代表本次检查。` };
  return { title: status === "stopped" ? "巡检已停止" : "没有可用结果", detail: "补全配置或重试失败项后再次巡检。" };
}

export function formatAuditTime(checkedAt: number) {
  if (!Number.isFinite(checkedAt)) return "时间未知";
  const value = new Date(checkedAt);
  if (Number.isNaN(value.getTime())) return "时间未知";
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())} ${pad(value.getHours())}:${pad(value.getMinutes())}`;
}
