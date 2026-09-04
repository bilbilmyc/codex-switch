import type { DeepValidationErrorCategory, DeepValidationResult } from "./types";

export type DeepValidationCheck =
  | { state: "running" }
  | { state: "result"; result: DeepValidationResult }
  | { state: "invoke_error"; message: string; checkedAtUnixMs: number };

export type DeepValidationPresentation = {
  tone: "checking" | "success" | "error";
  title: string;
  category: string;
  duration?: string;
  usage?: string;
  checkedAt?: string;
  detail?: string;
};

const errorCategoryLabels: Record<DeepValidationErrorCategory, string> = {
  missing_api_key: "缺少 API Key",
  invalid_base_url: "接口地址无效",
  unauthorized: "凭据未授权",
  rate_limited: "上游限流",
  upstream_error: "上游服务错误",
  request_timeout: "请求超时",
  network_error: "网络连接失败",
  response_too_large: "响应超过安全上限",
  invalid_response: "响应格式无效",
  request_rejected: "验证请求被拒绝",
};

export function deepValidationPresentation(check: DeepValidationCheck): DeepValidationPresentation {
  if (check.state === "running") {
    return {
      tone: "checking",
      title: "真实模型请求进行中",
      category: "完成前无法取消或切换配置",
    };
  }
  if (check.state === "invoke_error") {
    return {
      tone: "error",
      title: "验证未完成",
      category: "本地调用错误",
      checkedAt: formatDeepValidationTime(check.checkedAtUnixMs),
      detail: check.message,
    };
  }
  const { result } = check;
  if (result.status === "success") {
    return {
      tone: "success",
      title: "验证通过",
      category: "正常响应",
      duration: `${Math.max(0, Math.round(result.requestDurationMs))} ms`,
      usage: formatDeepValidationUsage(result.usage),
      checkedAt: formatDeepValidationTime(result.checkedAtUnixMs),
    };
  }
  return {
    tone: "error",
    title: "验证失败",
    category: result.errorCategory ? errorCategoryLabels[result.errorCategory] : "未知失败",
    duration: `${Math.max(0, Math.round(result.requestDurationMs))} ms`,
    usage: formatDeepValidationUsage(result.usage),
    checkedAt: formatDeepValidationTime(result.checkedAtUnixMs),
  };
}

export function deepValidationActionLabel(check?: DeepValidationCheck) {
  if (!check) return "发送验证请求";
  if (check.state === "running") return "正在等待模型";
  return check.state === "result" && check.result.status === "success" ? "再次验证" : "重试";
}

export function formatDeepValidationTime(unixMs: number) {
  if (!Number.isFinite(unixMs)) return "时间未知";
  const value = new Date(unixMs);
  if (Number.isNaN(value.getTime())) return "时间未知";
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())} ${pad(value.getHours())}:${pad(value.getMinutes())}:${pad(value.getSeconds())}`;
}

function formatDeepValidationUsage(usage: DeepValidationResult["usage"]) {
  if (!usage) return undefined;
  const parts: string[] = [];
  if (usage.totalTokens !== undefined) parts.push(`共 ${usage.totalTokens} tokens`);
  if (usage.inputTokens !== undefined) parts.push(`输入 ${usage.inputTokens}`);
  if (usage.outputTokens !== undefined) parts.push(`输出 ${usage.outputTokens}`);
  return parts.length ? parts.join(" · ") : undefined;
}
