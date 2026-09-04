import * as Dialog from "@radix-ui/react-dialog";
import { useQuery } from "@tanstack/react-query";
import {
  ArrowLeft,
  CircleAlert,
  CircleCheck,
  History,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  X,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import type {
  BackupApiKeyChange,
  BackupManagedChange,
  BackupPreview,
  BackupRouteSnapshot,
  BackupSnapshotSummary,
} from "./types";

type BackupCenterDialogProps = {
  open: boolean;
  locked: boolean;
  restorePending: boolean;
  restoreError?: string;
  onOpenChange: (open: boolean) => void;
  onClearRestoreError: () => void;
  onRestore: (backupId: string, liveRevision: string, createdAtUnixMs: number) => void;
};

export type BackupDiffRow = {
  key: string;
  label: string;
  state: BackupManagedChange | BackupApiKeyChange;
  current: string;
  target: string;
  sensitive?: boolean;
};

export function BackupCenterDialog({
  open,
  locked,
  restorePending,
  restoreError,
  onOpenChange,
  onClearRestoreError,
  onRestore,
}: BackupCenterDialogProps) {
  const [selectedId, setSelectedId] = useState<string>();
  const [reviewing, setReviewing] = useState(false);
  const [mobilePane, setMobilePane] = useState<"list" | "detail">("list");
  const rowRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const focusInitialRow = useRef(false);

  const center = useQuery({
    queryKey: ["backup-center"],
    queryFn: api.loadBackupCenter,
    enabled: open,
  });
  const backups = useMemo(
    () => [...(center.data?.backups ?? [])].sort((left, right) => right.createdAtUnixMs - left.createdAtUnixMs),
    [center.data?.backups],
  );
  const selected = backups.find((backup) => backup.id === selectedId);
  const preview = useQuery({
    queryKey: ["backup-preview", selectedId],
    queryFn: () => api.loadBackupPreview(selectedId!),
    enabled: open && Boolean(selectedId),
  });

  useEffect(() => {
    if (!open) {
      setSelectedId(undefined);
      setReviewing(false);
      setMobilePane("list");
      focusInitialRow.current = false;
      return;
    }
    focusInitialRow.current = true;
  }, [open]);

  useEffect(() => {
    if (!open || backups.length === 0) return;
    if (!selectedId || !backups.some((backup) => backup.id === selectedId)) {
      setSelectedId(backups[0].id);
      setReviewing(false);
    }
  }, [backups, open, selectedId]);

  useEffect(() => {
    if (!open || !selectedId || !focusInitialRow.current) return;
    const index = backups.findIndex((backup) => backup.id === selectedId);
    if (index < 0) return;
    focusInitialRow.current = false;
    requestAnimationFrame(() => rowRefs.current[index]?.focus());
  }, [backups, open, selectedId]);

  useEffect(() => {
    if (preview.isFetching && !restorePending) setReviewing(false);
  }, [preview.isFetching, restorePending]);

  const close = () => {
    if (!restorePending) onOpenChange(false);
  };
  const chooseBackup = (backup: BackupSnapshotSummary, openDetail: boolean) => {
    setSelectedId(backup.id);
    setReviewing(false);
    onClearRestoreError();
    if (openDetail) setMobilePane("detail");
  };
  const returnToList = () => {
    setReviewing(false);
    setMobilePane("list");
    const index = backups.findIndex((backup) => backup.id === selectedId);
    requestAnimationFrame(() => rowRefs.current[index]?.focus());
  };
  const reloadSelectedPreview = async () => {
    onClearRestoreError();
    setReviewing(false);
    await preview.refetch();
  };
  const moveSelection = (index: number, event: React.KeyboardEvent<HTMLButtonElement>) => {
    let nextIndex = index;
    if (event.key === "ArrowDown") nextIndex = Math.min(backups.length - 1, index + 1);
    else if (event.key === "ArrowUp") nextIndex = Math.max(0, index - 1);
    else if (event.key === "Home") nextIndex = 0;
    else if (event.key === "End") nextIndex = backups.length - 1;
    else return;
    event.preventDefault();
    const backup = backups[nextIndex];
    if (!backup) return;
    chooseBackup(backup, false);
    rowRefs.current[nextIndex]?.focus();
  };

  const titleDetail = center.isPending
    ? "正在读取备份记录"
    : center.isError
      ? "备份记录读取失败"
      : backups.length === 0
        ? "还没有自动快照"
        : `共 ${backups.length} 个快照 · 最多保留 10 个`;

  return <Dialog.Root open={open} onOpenChange={(next) => { if (next || !restorePending) onOpenChange(next); }}>
    <Dialog.Portal>
      <Dialog.Overlay className="legacy-dialog-overlay" />
      <Dialog.Content
        className={`legacy-backup-center mobile-${mobilePane}`}
        aria-describedby="backup-center-description"
        onEscapeKeyDown={(event) => {
          if (restorePending) {
            event.preventDefault();
          } else if (reviewing) {
            event.preventDefault();
            setReviewing(false);
          } else if (mobilePane === "detail") {
            event.preventDefault();
            returnToList();
          }
        }}
        onPointerDownOutside={(event) => { if (restorePending) event.preventDefault(); }}
      >
        <header className="legacy-backup-header">
          <History size={19} />
          <div>
            <Dialog.Title>备份中心</Dialog.Title>
            <Dialog.Description id="backup-center-description" asChild><span aria-live="polite">{titleDetail}</span></Dialog.Description>
          </div>
          <button type="button" title="关闭备份中心" aria-label="关闭备份中心" disabled={restorePending} onClick={close}><X size={17} /></button>
        </header>

        <div className="legacy-backup-body">
          <section className="legacy-backup-list-pane" aria-label="备份快照">
            {center.isPending ? <StatePanel icon={<RefreshCw className="spin" size={18} />} title="正在读取备份记录" detail="正在校验本地快照。" />
              : center.isError ? <StatePanel icon={<CircleAlert size={19} />} tone="error" title="备份记录无法读取" detail="没有修改任何 Codex 文件。" action={<button className="legacy-command-button" type="button" onClick={() => center.refetch()}><RefreshCw size={14} />重试</button>} />
                : backups.length === 0 ? <StatePanel icon={<History size={20} />} title="还没有备份" detail="应用中转站、同步上下文或执行恢复时会自动创建。" />
                  : <div className="legacy-backup-list" role="listbox" aria-label="按时间排列的备份快照">
                    {backups.map((backup, index) => <button
                      className={`legacy-backup-row ${backup.id === selectedId ? "selected" : ""}`}
                      type="button"
                      role="option"
                      aria-selected={backup.id === selectedId}
                      tabIndex={backup.id === selectedId ? 0 : -1}
                      disabled={locked}
                      key={backup.id}
                      ref={(element) => { rowRefs.current[index] = element; }}
                      onFocus={() => chooseBackup(backup, false)}
                      onClick={() => chooseBackup(backup, true)}
                      onKeyDown={(event) => moveSelection(index, event)}
                    >
                      <span className="legacy-backup-rail" aria-hidden="true"><i /></span>
                      <span className="legacy-backup-row-copy"><strong>{formatBackupLocalTime(backup.createdAtUnixMs)}</strong><small>{backupFilePresence(backup)}</small></span>
                      {index === 0 ? <em>最新</em> : null}
                    </button>)}
                  </div>}
          </section>

          <section className="legacy-backup-detail-pane" aria-label="所选快照恢复预览">
            <button className="legacy-backup-mobile-back" type="button" disabled={restorePending} onClick={returnToList}><ArrowLeft size={15} />返回备份列表</button>
            {!selected ? <StatePanel icon={<History size={20} />} title="选择一个快照" detail="查看它与当前 Codex 配置之间的脱敏差异。" />
              : preview.isPending ? <StatePanel icon={<RefreshCw className="spin" size={18} />} title="正在生成恢复预览" detail={formatBackupLocalTime(selected.createdAtUnixMs)} />
                : preview.isError ? <StatePanel icon={<CircleAlert size={19} />} tone="error" title="这个快照无法读取或校验" detail="不会执行恢复，可重新读取或选择其他快照。" action={<button className="legacy-command-button" type="button" onClick={() => preview.refetch()}><RefreshCw size={14} />重新读取</button>} />
                  : preview.data ? <BackupDetail preview={preview.data} reviewing={reviewing} restoreError={restoreError} onReload={() => void reloadSelectedPreview()} /> : null}
          </section>
        </div>

        <footer className="legacy-backup-footer">
          <div><strong>恢复前会创建回滚点</strong><span>当前配置、凭据、工具状态和模型目录会先另存。</span></div>
          <button className="legacy-command-button" type="button" disabled={restorePending} onClick={reviewing ? () => { setReviewing(false); onClearRestoreError(); } : close}>{reviewing ? "返回预览" : "关闭"}</button>
          {selected && preview.data ? <button
            className="legacy-command-button primary legacy-backup-detail-action"
            type="button"
            disabled={locked || preview.isFetching}
            onClick={() => {
              if (!reviewing) {
                setReviewing(true);
                onClearRestoreError();
                return;
              }
              onRestore(selected.id, preview.data.liveRevision, selected.createdAtUnixMs);
            }}
          >{restorePending ? <><RefreshCw className="spin" size={14} />正在恢复</> : reviewing ? <><RotateCcw size={14} />确认恢复</> : <><History size={14} />恢复此快照</>}</button> : null}
        </footer>
      </Dialog.Content>
    </Dialog.Portal>
  </Dialog.Root>;
}

function BackupDetail({ preview, reviewing, restoreError, onReload }: { preview: BackupPreview; reviewing: boolean; restoreError?: string; onReload: () => void }) {
  const rows = buildBackupDiffRows(preview);
  return <div className="legacy-backup-detail">
    <header className="legacy-backup-detail-head">
      <div><span>恢复预览</span><strong>{formatBackupLocalTime(preview.backup.createdAtUnixMs)}</strong></div>
      {preview.activeProfile ? <small>恢复后活动中转站 · {preview.activeProfile.name}</small> : <small>恢复后不关联已保存的中转站</small>}
    </header>

    {!preview.current ? <div className="legacy-backup-warning"><CircleAlert size={16} /><span>当前配置无法读取，受管字段差异可能显示为未知。</span></div> : null}
    {!preview.backup.configPresent ? <div className="legacy-backup-warning danger"><CircleAlert size={16} /><span>此快照不包含 config.toml，恢复后当前配置文件将被移除。</span></div> : null}
    {!preview.backup.authPresent ? <div className="legacy-backup-warning danger"><CircleAlert size={16} /><span>此快照不包含 auth.json，恢复后当前凭据文件将被移除。</span></div> : null}

    <section className="legacy-backup-section">
      <h3>受管配置差异</h3>
      {rows.length ? <div className="legacy-backup-diff" role="table" aria-label="当前配置与所选快照的脱敏差异">
        <div className="legacy-backup-diff-head" role="row"><span role="columnheader">项目</span><span role="columnheader">当前</span><span aria-hidden="true" /><span role="columnheader">快照</span></div>
        {rows.map((row) => <div className={`legacy-backup-diff-row ${row.state}`} role="row" key={row.key}>
          <strong role="rowheader">{row.label}</strong>
          <span role="cell">{row.current}</span>
          <i aria-hidden="true">→</i>
          <span role="cell">{row.target}</span>
        </div>)}
      </div> : <div className="legacy-backup-no-change"><CircleCheck size={16} /><span>受管配置与当前文件一致。</span></div>}
    </section>

    <section className="legacy-backup-section">
      <h3>文件影响</h3>
      <div className="legacy-backup-file-grid">
        <FileEffect label="config.toml" changed={preview.fileChanges.config} />
        <FileEffect label="auth.json" changed={preview.fileChanges.auth} />
        <FileEffect label="工具状态" changed={preview.fileChanges.state} />
        <FileEffect label="模型目录" changed={preview.fileChanges.catalog} />
      </div>
    </section>

    {reviewing ? <section className="legacy-backup-review" aria-live="polite">
      <ShieldCheck size={18} />
      <div><strong>确认恢复这个快照</strong><span>恢复开始前会保存当前 config.toml、auth.json、工具状态和模型目录。恢复完成后，可从备份中心切回该回滚点。</span></div>
    </section> : null}
    {restoreError ? <div className="legacy-backup-restore-error" role="alert"><CircleAlert size={16} /><div><span>{restoreError}</span><button className="legacy-command-button" type="button" onClick={onReload}><RefreshCw size={14} />重新读取</button></div></div> : null}
  </div>;
}

function FileEffect({ label, changed }: { label: string; changed: boolean }) {
  const presentation = changed
    ? { tone: "changed", label: "将恢复" }
    : { tone: "unchanged", label: "不变" };
  return <div className={`legacy-backup-file-effect ${presentation.tone}`}><span>{label}</span><strong>{presentation.label}</strong></div>;
}

function StatePanel({ icon, title, detail, tone = "neutral", action }: { icon: React.ReactNode; title: string; detail: string; tone?: "neutral" | "error"; action?: React.ReactNode }) {
  return <div className={`legacy-backup-state ${tone}`} role={tone === "error" ? "alert" : "status"}>{icon}<strong>{title}</strong><span>{detail}</span>{action}</div>;
}

export function buildBackupDiffRows(preview: BackupPreview): BackupDiffRow[] {
  const current = preview.current;
  const target = preview.target;
  const fields: Array<{
    key: string;
    label: string;
    state: BackupManagedChange;
    current: (value?: BackupRouteSnapshot) => string;
    target: (value: BackupRouteSnapshot) => string;
  }> = [
    { key: "provider", label: "中转站", state: preview.managedChanges.provider, current: (value) => displayOptional(value?.providerName), target: (value) => displayOptional(value.providerName) },
    { key: "baseUrl", label: "接口地址", state: preview.managedChanges.baseUrl, current: (value) => value ? value.baseUrlConfigured ? "已配置" : "未配置" : "无法读取", target: (value) => value.baseUrlConfigured ? "将恢复设置" : "未配置" },
    { key: "model", label: "默认模型", state: preview.managedChanges.model, current: (value) => displayOptional(value?.model), target: (value) => displayOptional(value.model) },
    { key: "reviewModel", label: "审查模型", state: preview.managedChanges.reviewModel, current: (value) => value?.reviewModel || "跟随默认模型", target: (value) => value.reviewModel || "跟随默认模型" },
    { key: "context", label: "上下文", state: preview.managedChanges.context, current: (value) => value?.contextSummary || "无法读取", target: (value) => value.contextSummary },
  ];
  const rows = fields
    .filter((field) => field.state !== "unchanged")
    .map<BackupDiffRow>((field) => ({
      key: field.key,
      label: field.label,
      state: field.state,
      current: field.current(current),
      target: field.target(target),
    }));
  if (preview.managedChanges.apiKey !== "unchanged") {
    rows.push(apiKeyDiffRow(preview.managedChanges.apiKey));
  }
  return rows;
}

export function formatBackupLocalTime(unixMs: number) {
  if (!Number.isFinite(unixMs)) return "时间未知";
  const value = new Date(unixMs);
  if (Number.isNaN(value.getTime())) return "时间未知";
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())} ${pad(value.getHours())}:${pad(value.getMinutes())}`;
}

function apiKeyDiffRow(change: BackupApiKeyChange): BackupDiffRow {
  const copy: Record<BackupApiKeyChange, { current: string; target: string }> = {
    unchanged: { current: "不变", target: "不变" },
    added: { current: "未保存", target: "将添加" },
    removed: { current: "已保存", target: "将移除" },
    replaced: { current: "已保存", target: "将替换" },
    unknown: { current: "无法判断", target: "内容不显示" },
  };
  return { key: "apiKey", label: "API Key", state: change, current: copy[change].current, target: copy[change].target, sensitive: true };
}

function displayOptional(value?: string) {
  return value?.trim() || "未设置";
}

function backupFilePresence(backup: BackupSnapshotSummary) {
  const files = [
    backup.configPresent ? "config.toml" : undefined,
    backup.authPresent ? "auth.json" : undefined,
    backup.statePresent ? "工具状态" : undefined,
    backup.catalogPresent ? "模型目录" : undefined,
  ].filter(Boolean);
  return files.length ? files.join(" · ") : "空快照";
}
