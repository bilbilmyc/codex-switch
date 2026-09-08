import { CircleAlert, CircleCheck, CircleX, Info, RefreshCw } from "lucide-react";

export function StatusIcon({ busy, tone }: { busy: boolean; tone?: "success" | "warning" | "error" }) {
  const Icon = busy ? RefreshCw : tone === "success" ? CircleCheck
    : tone === "warning" ? CircleAlert : tone === "error" ? CircleX : Info;
  return <Icon className={`legacy-status-icon${busy ? " spin" : ""}`} size={18} strokeWidth={2} aria-hidden="true" />;
}
