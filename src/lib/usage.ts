import type { OfficialAccountUsage, ProviderUsage } from "../types";

export interface UsageView<T> {
  checking: boolean;
  data: T | null;
  refreshError?: string;
}

export function mergeOfficialUsage(previous: UsageView<OfficialAccountUsage> | undefined, data: OfficialAccountUsage): UsageView<OfficialAccountUsage> {
  if (data.health === "unknown" && previous?.data?.health === "valid") {
    return { checking: false, data: previous.data, refreshError: data.message ?? "额度暂时无法查询" };
  }
  return { checking: false, data };
}

export function mergeProviderUsage(previous: UsageView<ProviderUsage> | undefined, data: ProviderUsage): UsageView<ProviderUsage> {
  if (data.status === "unknown" && previous?.data?.status === "available") {
    return { checking: false, data: previous.data, refreshError: data.message ?? "余额暂时无法查询" };
  }
  return { checking: false, data };
}

export function updatedLabel(queriedAt: number, now: number): string {
  const minutes = Math.max(0, Math.floor((now - queriedAt) / 60_000));
  if (minutes === 0) return "刚刚更新";
  if (minutes < 60) return `${minutes} 分钟前更新`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前更新`;
  return `${Math.floor(hours / 24)} 天前更新`;
}

export function resetTimeLabel(resetAt: number | null, now: number): string | null {
  if (!resetAt) return null;
  const date = new Date(resetAt * 1000);
  if (Number.isNaN(date.getTime())) return null;
  const absolute = new Intl.DateTimeFormat("zh-CN", {
    year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false,
  }).format(date);
  const minutes = Math.max(0, Math.ceil((date.getTime() - now) / 60_000));
  const relative = minutes === 0 ? "已到重置时间，请刷新额度"
    : minutes < 60 ? `还有 ${minutes} 分钟重置`
    : `还有 ${Math.floor(minutes / 60)} 小时${minutes % 60 ? ` ${minutes % 60} 分钟` : ""}重置`;
  return `${relative}（${absolute}）`;
}
