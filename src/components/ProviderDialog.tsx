import { Eye, EyeOff, Globe2, KeyRound, Tag, X } from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import type { ProviderDraft, ProviderSummary } from "../types";

interface ProviderDialogProps {
  provider: ProviderSummary | null;
  busy: boolean;
  onCancel: () => void;
  onSubmit: (draft: ProviderDraft) => void;
}

export function ProviderDialog({
  provider,
  busy,
  onCancel,
  onSubmit,
}: ProviderDialogProps) {
  const [name, setName] = useState("");
  const [apiUrl, setApiUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [showKey, setShowKey] = useState(false);

  useEffect(() => {
    setName(provider?.name ?? "");
    setApiUrl(provider?.apiUrl ?? "");
    setApiKey("");
    setShowKey(false);
  }, [provider]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    onSubmit({ name, apiUrl, apiKey });
  };

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={onCancel}>
      <form
        className="dialog-panel provider-form"
        role="dialog"
        aria-modal="true"
        aria-labelledby="provider-dialog-title"
        onSubmit={submit}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="dialog-heading">
          <h2 id="provider-dialog-title">{provider ? "编辑供应商" : "添加供应商"}</h2>
          <button
            className="icon-button"
            type="button"
            aria-label="关闭"
            title="关闭"
            disabled={busy}
            onClick={onCancel}
          >
            <X size={18} />
          </button>
        </div>

        <label htmlFor="provider-name">名称</label>
        <div className="provider-field">
          <Tag size={16} aria-hidden="true" />
          <input
            id="provider-name"
            autoFocus
            maxLength={80}
            autoComplete="organization"
            placeholder="例如：小猪窝"
            required
            disabled={busy}
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </div>

        <label htmlFor="api-url">API URL</label>
        <div className="provider-field">
          <Globe2 size={16} aria-hidden="true" />
          <input
            id="api-url"
            inputMode="url"
            autoComplete="url"
            placeholder="https://api.example.com"
            required
            disabled={busy}
            value={apiUrl}
            onChange={(event) => setApiUrl(event.target.value)}
          />
        </div>

        <label htmlFor="api-key">API Key</label>
        <div className="provider-field">
          <KeyRound size={16} aria-hidden="true" />
          <input
            id="api-key"
            type={showKey ? "text" : "password"}
            autoComplete="off"
            placeholder={provider?.hasApiKey ? "已配置，留空继续使用" : "输入 API Key"}
            required={!provider?.hasApiKey}
            disabled={busy}
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
          />
          <button
            className="secret-toggle"
            type="button"
            aria-label={showKey ? "隐藏 API Key" : "显示 API Key"}
            title={showKey ? "隐藏 API Key" : "显示 API Key"}
            disabled={busy}
            onClick={() => setShowKey((visible) => !visible)}
          >
            {showKey ? <EyeOff size={18} /> : <Eye size={18} />}
          </button>
        </div>

        <div className="dialog-actions">
          <button className="secondary-button" type="button" disabled={busy} onClick={onCancel}>
            取消
          </button>
          <button className="primary-button" type="submit" disabled={busy}>
            {busy ? "验证中" : "保存"}
          </button>
        </div>
      </form>
    </div>
  );
}
