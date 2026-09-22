import {
  AlertCircle,
  Check,
  KeyRound,
  LoaderCircle,
  Pencil,
  Route,
  RefreshCw,
  Trash2,
} from "lucide-react";
import type { ProviderSummary, ProviderUsage } from "../types";

interface ProviderCardProps {
  provider: ProviderSummary;
  usage: ProviderUsageView | undefined;
  disabled: boolean;
  activating: boolean;
  onActivate: () => void;
  onEdit: () => void;
  onRefreshModels: () => void;
  onRefreshUsage: () => void;
  onDelete: () => void;
  onEnableRouting: () => void;
}

export interface ProviderUsageView {
  checking: boolean;
  data: ProviderUsage | null;
}

function amount(value: number, unit: string | null): string {
  const digits = Math.abs(value) >= 1000 ? 0 : 2;
  const formatted = value.toLocaleString("zh-CN", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
  return unit === "USD" ? `$${formatted}` : `${formatted}${unit ? ` ${unit}` : ""}`;
}

function providerHost(apiUrl: string): string {
  try {
    return new URL(apiUrl).host;
  } catch {
    return apiUrl.replace(/^https?:\/\//i, "").replace(/\/.*$/, "");
  }
}

function balanceText(usage: ProviderUsageView | undefined): string | null {
  const data = usage?.data;
  if (!data || data.status !== "available") return null;
  if (data.unlimited) return "不限额";
  if (data.balance != null) return amount(data.balance, data.unit);
  if (data.total != null && data.used != null) {
    return amount(Math.max(0, data.total - data.used), data.unit);
  }
  return null;
}

function ProviderUsagePanel({
  provider,
  usage,
  disabled,
  onRefresh,
}: {
  provider: ProviderSummary;
  usage: ProviderUsageView | undefined;
  disabled: boolean;
  onRefresh: () => void;
}) {
  if (!provider.hasApiKey) return null;
  if (!usage || (usage.checking && !usage.data)) {
    return (
      <div className="provider-usage-status checking" aria-live="polite">
        <LoaderCircle className="spinner" size={13} />
        <span>正在查询余额</span>
      </div>
    );
  }

  const data = usage.data;
  if (!data || data.status === "unsupported") return null;
  if (data.status !== "available") {
    const text = data.status === "unauthorized"
      ? "API Key 验证失败"
      : (data.message ?? "余额暂时无法查询");
    return (
      <div className="provider-usage-status unavailable" title={data.message ?? undefined}>
        <AlertCircle size={14} />
        <span>{text}</span>
        <button
          className="quota-refresh"
          type="button"
          disabled={disabled || usage.checking}
          aria-label={`重新查询 ${provider.name} 的余额`}
          onClick={onRefresh}
        >
          <RefreshCw className={usage.checking ? "spinner" : ""} size={13} />
        </button>
      </div>
    );
  }

  return null;
}

export function ProviderCard({
  provider,
  usage,
  disabled,
  activating,
  onActivate,
  onEdit,
  onRefreshModels,
  onRefreshUsage,
  onDelete,
  onEnableRouting,
}: ProviderCardProps) {
  const needsRouting =
    provider.protocol !== "openai_responses" && provider.routingMode !== "local";
  const balance = balanceText(usage);
  const displayName = `${provider.name} (${providerHost(provider.apiUrl)})`;

  return (
    <article className={`provider-card${provider.active ? " active" : ""}`}>
      <button
        className="provider-select"
        type="button"
        disabled={disabled}
        aria-label={provider.active ? `重新应用 ${provider.name}` : `切换到 ${provider.name}`}
        onClick={onActivate}
      >
        <span className="provider-icon api-icon" aria-hidden="true">
          <KeyRound size={19} />
        </span>
        <span className="provider-copy">
          <span className="provider-title-row">
            <strong title={`${provider.name} · ${provider.apiUrl}`}>{displayName}</strong>
            {balance && <span className="provider-inline-balance">{balance}</span>}
            {provider.active && (
              <span className="active-badge">
                <Check size={12} /> 当前
              </span>
            )}
          </span>
          <span className="provider-meta">
            <span>{provider.modelCount} 个模型</span>
          </span>
        </span>
        {activating && <LoaderCircle className="spinner" size={18} aria-label="处理中" />}
      </button>

      <div className="provider-actions">
        <button className="icon-button" type="button" aria-label={`刷新 ${provider.name} 的模型列表`} title="刷新模型列表" disabled={disabled} onClick={onRefreshModels}>
          <RefreshCw size={16} />
        </button>
        {needsRouting && (
          <button
            className="icon-button route-button"
            type="button"
            aria-label={`为 ${provider.name} 启用本地路由`}
            title="启用本地路由"
            disabled={disabled}
            onClick={onEnableRouting}
          >
            <Route size={17} />
          </button>
        )}
        <button
          className="icon-button"
          type="button"
          aria-label={`编辑 ${provider.name}`}
          title="编辑"
          disabled={disabled}
          onClick={onEdit}
        >
          <Pencil size={16} />
        </button>
        <button
          className="icon-button delete-button"
          type="button"
          aria-label={`删除 ${provider.name}`}
          title={provider.active ? "当前供应商不能删除" : "删除"}
          disabled={disabled || provider.active}
          onClick={onDelete}
        >
          <Trash2 size={16} />
        </button>
      </div>
      <ProviderUsagePanel
        provider={provider}
        usage={usage}
        disabled={disabled}
        onRefresh={onRefreshUsage}
      />
    </article>
  );
}
