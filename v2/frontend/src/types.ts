export type ProfileDraft = {
  name: string;
  baseUrl: string;
  apiKey?: string;
  clearApiKey?: boolean;
  model: string;
  reviewModel?: string;
};

export type ProfileApplyState =
  | "inactive"
  | "applied"
  | "pending_changes"
  | "external_drift"
  | "unknown";

export type ProfileSummary = {
  id: string;
  name: string;
  baseUrl: string;
  model: string;
  reviewModel?: string;
  hasApiKey: boolean;
  isActive: boolean;
  applyState: ProfileApplyState;
};

export type Bootstrap = {
  profiles: ProfileSummary[];
  canRestore: boolean;
  startupMessage?: string;
};

export type BackupSnapshotSummary = {
  id: string;
  createdAtUnixMs: number;
  configPresent: boolean;
  authPresent: boolean;
  statePresent: boolean;
  catalogPresent: boolean;
};

export type BackupCenterView = {
  backups: BackupSnapshotSummary[];
};

export type BackupRouteSnapshot = {
  providerName?: string;
  baseUrlConfigured: boolean;
  model?: string;
  reviewModel?: string;
  contextSummary: string;
  hasApiKey: boolean;
};

export type BackupManagedChange = "changed" | "unchanged" | "unknown";
export type BackupApiKeyChange = "unchanged" | "added" | "removed" | "replaced" | "unknown";

export type BackupPreview = {
  backup: BackupSnapshotSummary;
  liveRevision: string;
  current?: BackupRouteSnapshot;
  target: BackupRouteSnapshot;
  managedChanges: {
    provider: BackupManagedChange;
    baseUrl: BackupManagedChange;
    model: BackupManagedChange;
    reviewModel: BackupManagedChange;
    context: BackupManagedChange;
    apiKey: BackupApiKeyChange;
  };
  fileChanges: {
    config: boolean;
    auth: boolean;
    state: boolean;
    catalog: boolean;
  };
  activeProfile?: {
    id: string;
    name: string;
  };
};

export type ModelListView = {
  models: string[];
  cacheLabel: string;
};

export type DeepValidationErrorCategory =
  | "missing_api_key"
  | "invalid_base_url"
  | "unauthorized"
  | "rate_limited"
  | "upstream_error"
  | "request_timeout"
  | "network_error"
  | "response_too_large"
  | "invalid_response"
  | "request_rejected";

export type DeepValidationResult = {
  status: "success" | "failed";
  requestDurationMs: number;
  checkedAtUnixMs: number;
  errorCategory?: DeepValidationErrorCategory;
  usage?: {
    inputTokens?: number;
    outputTokens?: number;
    totalTokens?: number;
  };
};

export type ContextDraft = {
  useDefaults: boolean;
  windowK: string;
  compactPercent: number;
};

export type ContextView = ContextDraft & {
  summary: string;
  isActive: boolean;
  syncState: "synced" | "unsynced" | "saved_for_switch" | "inherited_live";
  status: string;
  budget: {
    recentSession: string;
    instructionTokens: string;
    availableBudget: string;
    historyRatio: number;
    instructionRatio: number;
    remainingRatio: number;
    suggestedWindowK: string;
    instructions: Array<{ name: string; detail: string }>;
  };
};

export type UsageValue = {
  input: string;
  cached: string;
  output: string;
  calls: string;
};

export type UsageView = {
  period: "today" | "last_7_days" | "last_30_days";
  current: UsageValue;
  todaySummary: string;
  periodTotal: UsageValue;
  trend: Array<{ label: string; input: string; output: string; inputRatio: number; outputRatio: number; usage: UsageValue }>;
  models: Array<{ model: string; input: string; cached: string; output: string; calls: string }>;
  hasData: boolean;
  status: string;
};

export type ConfirmationIntent = "primary" | "danger" | "neutral";

export type Confirmation = {
  token: string;
  title: string;
  detail: string;
  options: Array<{
    id: string;
    label: string;
    intent: ConfirmationIntent;
  }>;
};

export type ApplyResponse =
  | { kind: "applied"; activeProfileId: string; warning?: string }
  | { kind: "requires_confirmation"; confirmation: Confirmation }
  | { kind: "imported_current"; profile: ProfileSummary; warning?: string }
  | { kind: "restored"; activeProfileId?: string; warning?: string }
  | { kind: "context_saved"; context: ContextView; warning?: string };
