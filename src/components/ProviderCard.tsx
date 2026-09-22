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

const protocolLabels: Record<string, string> = {
  openai_responses: "Responses",
  openai_chat: "Chat Completions",
  anthropic_messages: "Anthropic Messages",
};

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

  const title = [data.system, data.plan].filter(Boolean).join(" · ");
  const primary = data.unlimited
    ? "不限额度"
    : data.balance != null
      ? amount(data.balance, data.unit)
      : "余额接口已连接";
  const detail = data.total != null && data.used != null
    ? `总额 ${amount(data.total, data.unit)} · 已用 ${amount(data.used, data.unit)}`
    : data.message;

  return (
    <div className="provider-balance">
      <span className="provider-balance-name" title={title}>{title}</span>
      <span className="provider-balance-value">
        <strong>{primary}</strong>
        {detail && <small>{detail}</small>}
      </span>
      <button
        className="quota-refresh"
        type="button"
        disabled={disabled || usage.checking}
        aria-label={`刷新 ${provider.name} 的余额`}
        onClick={onRefresh}
      >
        <RefreshCw className={usage.checking ? "spinner" : ""} size={13} />
      </button>
    </div>
  );
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
  const protocol = protocolLabels[provider.protocol] ?? provider.protocol;

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
            <strong>{provider.name}</strong>
            {provider.active && (
              <span className="active-badge">
                <Check size={12} /> 当前
              </span>
            )}
          </span>
          <span className="provider-meta" title={provider.apiUrl}>
            <span>{provider.apiUrl}</span>
            <span aria-hidden="true">·</span>
            <span>{provider.modelCount} 个模型</span>
            <span aria-hidden="true">·</span>
            <span>{protocol}</span>
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
