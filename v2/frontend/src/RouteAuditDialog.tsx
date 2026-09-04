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
  profileConfigurationIssue,
  summarizeRouteAudit,
  type RouteAuditEntry,
  type RouteAuditSession,
} from "./route-audit";
import type { ProfileSummary } from "./types";

export type RouteAuditStatus = "idle" | "running" | "stopping" | "retrying" | "complete" | "stopped";

type RouteAuditActions = {
  start: () => void;
  stop: () => void;
  retry: (profile: ProfileSummary) => void;
  edit: (profile: ProfileSummary) => void;
  apply: (profile: ProfileSummary) => void;
};

type RouteAuditDialogProps = {
  open: boolean;
  profiles: ProfileSummary[];
  session?: RouteAuditSession;
  status: RouteAuditStatus;
  locked: boolean;
  onOpenChange: (open: boolean) => void;
  actions: RouteAuditActions;
};

export function RouteAuditDialog({ open, profiles, session, status, locked, onOpenChange, actions }: RouteAuditDialogProps) {
  const profileById = useMemo(
    () => new Map(profiles.map((profile) => [profile.id, profile])),
    [profiles],
  );
  const entries = useMemo(() => {
    if (session) return session.entries.filter((entry) => profileById.has(entry.id));
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
  const headline = auditHeadline(status, summary.total, resolved, summary.success, summary.error, summary.incomplete);
  const footer = auditFooter(status, summary);

  return <Dialog.Root open={open} onOpenChange={onOpenChange}><Dialog.Portal><Dialog.Overlay className="legacy-dialog-overlay" /><Dialog.Content className="legacy-route-audit" aria-describedby={undefined}>
    <header className="legacy-audit-header">
      <Activity size={19} />
      <div><Dialog.Title>全路由巡检</Dialog.Title><span aria-live="polite">{headline}</span></div>
      <button type="button" title="关闭巡检" aria-label="关闭巡检" onClick={() => onOpenChange(false)}><X size={17} /></button>
    </header>
    <div className="legacy-audit-list">
      {entries.length ? entries.map((entry) => {
        const profile = profileById.get(entry.id);
        if (!profile) return null;
        return <AuditRow key={entry.id} entry={entry} profile={profile} locked={locked || running} actions={actions} />;
      }) : <div className="legacy-audit-empty"><strong>还没有中转站</strong><span>创建中转站后即可一次检查全部路由。</span></div>}
    </div>
    <footer className="legacy-audit-footer">
      <div><strong>{footer.title}</strong><span>{footer.detail}</span></div>
      <button className="legacy-command-button" type="button" onClick={() => onOpenChange(false)}>关闭</button>
      {running
        ? status === "retrying"
          ? <button className="legacy-command-button" type="button" disabled><RefreshCw className="spin" size={14} />正在重试</button>
          : <button className="legacy-command-button" type="button" disabled={status === "stopping"} onClick={actions.stop}><Square size={14} />{status === "stopping" ? "正在停止" : "停止"}</button>
        : <button className="legacy-command-button primary" type="button" disabled={locked || profiles.length === 0} onClick={actions.start}><Activity size={15} />{status === "idle" ? "开始巡检" : "重新巡检"}</button>}
    </footer>
  </Dialog.Content></Dialog.Portal></Dialog.Root>;
}

function AuditRow({ entry, profile, locked, actions }: { entry: RouteAuditEntry; profile: ProfileSummary; locked: boolean; actions: RouteAuditActions }) {
  const presentation = auditEntryPresentation(entry);
  const applyState = describeApplyState(profile.applyState);
  const canRetry = entry.state === "success" || entry.state === "error" || entry.state === "stopped";
  const retryVisible = canRetry || entry.state === "checking";
  const retryLocked = locked || entry.state === "checking";
  const canApply = entry.state === "success";

  return <div className={`legacy-audit-row ${presentation.tone}`} aria-label={`${profile.name}，${presentation.label}`}>
    <div className="legacy-audit-row-icon">{presentation.icon}</div>
    <div className="legacy-audit-profile">
      <strong>{profile.name}</strong>
      <span>{routeHost(profile.baseUrl)} · {profile.model || "未设置模型"}</span>
    </div>
    <div className="legacy-audit-result">
      <strong>{presentation.label}</strong>
      <span>{presentation.detail}</span>
    </div>
    <div className="legacy-audit-actions" role="group" aria-label={`${profile.name} 巡检操作`}>
      {retryVisible ? <button type="button" title={entry.state === "checking" ? `正在检查 ${profile.name}` : `重新检查 ${profile.name}`} aria-label={entry.state === "checking" ? `正在检查 ${profile.name}` : `重新检查 ${profile.name}`} aria-disabled={retryLocked} onClick={() => { if (!retryLocked) actions.retry(profile); }}><RefreshCw className={entry.state === "checking" ? "spin" : undefined} size={15} /></button> : null}
      <button type="button" title={`编辑 ${profile.name}`} aria-label={`编辑 ${profile.name}`} disabled={locked} onClick={() => actions.edit(profile)}><Pencil size={15} /></button>
      {canApply ? <button className="apply" type="button" title={`${applyState.action}：${profile.name}`} aria-label={`${applyState.action}：${profile.name}`} disabled={locked} onClick={() => actions.apply(profile)}><Route size={15} /></button> : null}
    </div>
  </div>;
}

function auditEntryPresentation(entry: RouteAuditEntry) {
  switch (entry.state) {
    case "checking":
      return { tone: "checking", label: "正在检查", detail: "正在读取模型目录", icon: <RefreshCw className="spin" size={17} /> };
    case "success":
      return { tone: "success", label: "连接可用", detail: `${entry.latencyMs} ms · ${entry.models.models.length} 个模型 · ${formatAuditTime(entry.checkedAt)}`, icon: <CircleCheck size={17} /> };
    case "error":
      return { tone: "error", label: "连接失败", detail: entry.message, icon: <CircleAlert size={17} /> };
    case "incomplete":
      return { tone: "incomplete", label: "配置未完整", detail: entry.issue, icon: <CircleAlert size={17} /> };
    case "stopped":
      return { tone: "stopped", label: "本轮未检查", detail: "巡检已停止", icon: <Square size={14} /> };
    case "queued":
      return { tone: "queued", label: "等待检查", detail: "将在前一项完成后开始", icon: <Circle size={14} /> };
  }
}

function auditHeadline(status: RouteAuditStatus, total: number, resolved: number, success: number, error: number, incomplete: number) {
  if (status === "idle") return `${total} 个中转站 · 尚未巡检`;
  if (status === "running") return `正在检查 ${Math.min(resolved + 1, total)}/${total}`;
  if (status === "stopping") return `完成当前检查后停止 · ${resolved}/${total}`;
  if (status === "retrying") return `正在重试 · ${resolved}/${total} 已完成`;
  const prefix = status === "stopped" ? "巡检已停止" : "巡检完成";
  return `${prefix} · ${success} 可用 / ${incomplete} 未配置 / ${error} 失败`;
}

function auditFooter(status: RouteAuditStatus, summary: ReturnType<typeof summarizeRouteAudit>) {
  if (status === "idle") return { title: "检查已保存的连接", detail: "巡检不会自动切换或修改 Codex 配置。" };
  if (status === "running" || status === "stopping" || status === "retrying") return { title: `${summary.success + summary.error + summary.incomplete}/${summary.total} 已完成`, detail: "结果会逐项写回侧栏状态。" };
  if (summary.fastest) return { title: `本次最快 · ${summary.fastest.name}`, detail: `${summary.fastest.latencyMs} ms，仅代表本次检查。` };
  return { title: status === "stopped" ? "巡检已停止" : "没有可用结果", detail: "补全配置或重试失败项后再次巡检。" };
}

function formatAuditTime(checkedAt: number) {
  return new Date(checkedAt).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}
