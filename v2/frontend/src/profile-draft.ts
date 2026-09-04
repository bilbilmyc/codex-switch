import { z } from "zod";
import type { ProfileDraft } from "./types";

export const profileSchema = z.object({
  name: z.string().trim().min(1, "请输入中转站名称"),
  baseUrl: z
    .string()
    .trim()
    .url("请输入完整的 URL")
    .refine((value) => value.startsWith("https://") || value.startsWith("http://"), {
      message: "URL 必须以 http:// 或 https:// 开始",
    }),
  apiKey: z.string().optional(),
  clearApiKey: z.boolean().optional(),
  model: z.string().trim().min(1, "请输入默认模型"),
  reviewModel: z.string().optional(),
});

export type ProfileFormValues = z.infer<typeof profileSchema>;

export const emptyProfileForm: ProfileFormValues = {
  name: "",
  baseUrl: "",
  apiKey: "",
  clearApiKey: false,
  model: "",
  reviewModel: "",
};

export function toProfileDraft(values: ProfileFormValues): ProfileDraft {
  return {
    name: values.name,
    baseUrl: values.baseUrl,
    apiKey: values.apiKey?.trim() || undefined,
    clearApiKey: Boolean(values.clearApiKey),
    model: values.model,
    reviewModel: values.reviewModel?.trim() || undefined,
  };
}
