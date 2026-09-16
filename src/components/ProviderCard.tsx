import {
  Check,
  KeyRound,
  LoaderCircle,
  Pencil,
  Route,
  RefreshCw,
  Trash2,
} from "lucide-react";
import type { ProviderSummary } from "../types";

const protocolLabels: Record<string, string> = {
  openai_responses: "Responses",
  openai_chat: "Chat Completions",
  anthropic_messages: "Anthropic Messages",
};

interface ProviderCardProps {
  provider: ProviderSummary;
  disabled: boolean;
  activating: boolean;
  onActivate: () => void;
  onEdit: () => void;
  onRefreshModels: () => void;
  onDelete: () => void;
  onEnableRouting: () => void;
}

export function ProviderCard({
  provider,
  disabled,
  activating,
  onActivate,
  onEdit,
  onRefreshModels,
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
    </article>
  );
}
