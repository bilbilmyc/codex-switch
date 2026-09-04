export type ProfileDraft = {
  name: string;
  baseUrl: string;
  apiKey?: string;
  clearApiKey?: boolean;
  model: string;
  reviewModel?: string;
};

export type ProfileSummary = {
  id: string;
  name: string;
  baseUrl: string;
  model: string;
  reviewModel?: string;
  hasApiKey: boolean;
  isActive: boolean;
};

export type Bootstrap = {
  profiles: ProfileSummary[];
  canRestore: boolean;
  startupMessage?: string;
};

export type ModelListView = {
  models: string[];
  cacheLabel: string;
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
