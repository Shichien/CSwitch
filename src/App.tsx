import {
  Check,
  CircleUserRound,
  LoaderCircle,
  LogIn,
  Plus,
  ShieldCheck,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { ProviderCard } from "./components/ProviderCard";
import { ProviderDialog } from "./components/ProviderDialog";
import { cswitchApi } from "./lib/api";
import type {
  OperationProgress,
  ProviderDraft,
  ProviderState,
  ProviderSummary,
  SavedProvider,
} from "./types";

const EMPTY_STATE: ProviderState = {
  providers: [],
  activeProviderId: null,
  officialActive: false,
  keepOfficialAuth: false,
  officialAuthAvailable: false,
};

type Notice = { kind: "success" | "error"; text: string } | null;
type BusyAction = "load" | "official" | "save" | "route" | "delete" | "keep-auth" | string | null;

function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

function switchedNotice(name: string, synced: number): string {
  if (synced > 0) return `已切换到 ${name}，已同步 ${synced} 个任务`;
  return `已切换到 ${name}`;
}

function App() {
  const [state, setState] = useState<ProviderState>(EMPTY_STATE);
  const [busy, setBusy] = useState<BusyAction>("load");
  const [notice, setNotice] = useState<Notice>(null);
  const [providerDialogOpen, setProviderDialogOpen] = useState(false);
  const [editingProvider, setEditingProvider] = useState<ProviderSummary | null>(null);
  const [routingProvider, setRoutingProvider] = useState<ProviderSummary | null>(null);
  const [deletingProvider, setDeletingProvider] = useState<ProviderSummary | null>(null);
  const [progress, setProgress] = useState<OperationProgress | null>(null);
  const noticeTimer = useRef<number | null>(null);

  const showNotice = useCallback((kind: "success" | "error", text: string) => {
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    setNotice({ kind, text });
    noticeTimer.current = window.setTimeout(() => setNotice(null), 3000);
  }, []);

  const refresh = useCallback(async () => {
    setState(await cswitchApi.listProviders());
  }, []);

  useEffect(() => {
    let active = true;
    cswitchApi
      .listProviders()
      .then((next) => {
        if (active) setState(next);
      })
      .catch((error) => {
        if (active) showNotice("error", errorText(error));
      })
      .finally(() => {
        if (active) setBusy(null);
      });
    return () => {
      active = false;
      if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    };
  }, [showNotice]);

  useEffect(() => {
    let unlistenProgress: (() => void) | undefined;
    let unlistenChanged: (() => void) | undefined;
    let unlistenError: (() => void) | undefined;
    void cswitchApi.onProgress((next) => {
      setProgress(next.done ? null : next);
      if (next.done) setBusy(null);
      else if (next.operation) setBusy(next.operation);
    }).then((fn) => {
      unlistenProgress = fn;
    });
    void cswitchApi.onProvidersChanged(() => {
      void refresh();
    }).then((fn) => {
      unlistenChanged = fn;
    });
    void cswitchApi.onOperationError((message) => {
      showNotice("error", message);
    }).then((fn) => {
      unlistenError = fn;
    });
    return () => {
      unlistenProgress?.();
      unlistenChanged?.();
      unlistenError?.();
    };
  }, [refresh, showNotice]);

  const openAdd = () => {
    setEditingProvider(null);
    setProviderDialogOpen(true);
  };

  const openEdit = (provider: ProviderSummary) => {
    setEditingProvider(provider);
    setProviderDialogOpen(true);
  };

  const activate = async (provider: ProviderSummary) => {
    if (provider.protocol !== "openai_responses" && provider.routingMode !== "local") {
      setRoutingProvider(provider);
      return;
    }
    setBusy(provider.id);
    setProgress({
      operation: "activate",
      title: `切换到 ${provider.name}`,
      stage: "准备切换",
      detail: "正在获取操作锁并恢复未完成的操作。",
      current: 0,
      total: 7,
      done: false,
    });
    try {
      const report = await cswitchApi.activateProvider(provider.id);
      await refresh();
      showNotice("success", switchedNotice(provider.name, report.rolloutFilesUpdated));
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      setProgress(null);
      setBusy(null);
    }
  };

  const save = async (draft: ProviderDraft) => {
    const wasActive = Boolean(editingProvider?.active);
    setBusy("save");
    setProgress({
      operation: "save",
      title: "保存供应商",
      stage: "验证供应商",
      detail: "正在探测上游协议并拉取模型目录，可能需要几秒。",
      current: 1,
      total: 2,
      done: false,
    });
    try {
      const result: SavedProvider = await cswitchApi.saveProvider(editingProvider?.id ?? null, draft);
      setProviderDialogOpen(false);
      setEditingProvider(null);
      await refresh();
      if (result.routingRequired) {
        setRoutingProvider(result.provider);
        showNotice("success", "供应商已保存");
        return;
      }
      if (wasActive) {
        await cswitchApi.activateProvider(result.provider.id);
        await refresh();
      }
      showNotice("success", wasActive ? "供应商已更新并重新应用" : "供应商已保存");
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      setProgress(null);
      setBusy(null);
    }
  };

  const enableRouting = async () => {
    if (!routingProvider) return;
    const provider = routingProvider;
    setBusy("route");
    setProgress({
      operation: "activate",
      title: `切换到 ${provider.name}`,
      stage: "启用本地路由",
      detail: "正在启用协议转换并切换供应商。",
      current: 0,
      total: 7,
      done: false,
    });
    try {
      await cswitchApi.enableProviderRouting(provider.id);
      const report = await cswitchApi.activateProvider(provider.id);
      setRoutingProvider(null);
      await refresh();
      showNotice("success", switchedNotice(provider.name, report.rolloutFilesUpdated));
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      setProgress(null);
      setBusy(null);
    }
  };

  const removeProvider = async () => {
    if (!deletingProvider) return;
    setBusy("delete");
    try {
      await cswitchApi.deleteProvider(deletingProvider.id);
      setDeletingProvider(null);
      await refresh();
      showNotice("success", "供应商已删除");
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      setBusy(null);
    }
  };

  const useOfficial = async () => {
    if (busy || progress) return;
    setBusy("official");
    setProgress({
      operation: "official",
      title: "切换到官方登录",
      stage: "准备切换",
      detail: "正在检查官方登录状态。",
      current: 0,
      total: 7,
      done: false,
    });
    try {
      const report = await cswitchApi.startOfficialLogin();
      await refresh();
      showNotice("success", switchedNotice("官方登录", report.rolloutFilesUpdated));
    } catch (error) {
      const message = errorText(error);
      if (message !== "官方登录已取消") showNotice("error", message);
    } finally {
      setProgress(null);
      setBusy(null);
    }
  };

  const toggleKeepOfficialAuth = async () => {
    if (busy || progress) return;
    const enabled = !state.keepOfficialAuth;
    setBusy("keep-auth");
    setProgress({
      operation: "keep-auth",
      title: enabled ? "开启保留官方登录" : "关闭保留官方登录",
      stage: "准备更新",
      detail: "正在保存鉴权方式并重新应用当前供应商。",
      current: 0,
      total: 3,
      done: false,
    });
    try {
      setState(await cswitchApi.setKeepOfficialAuth(enabled));
      showNotice(
        "success",
        enabled
          ? "已开启：切换第三方时保留官方登录，请求走 API Key"
          : "已关闭：切换第三方时 API Key 直连",
      );
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      setProgress(null);
      setBusy(null);
    }
  };

  const cancelOfficial = async () => {
    try {
      await cswitchApi.cancelOfficialLogin();
      showNotice("success", "已取消官方登录");
    } catch (error) {
      showNotice("error", errorText(error));
    }
  };

  const isLoading = busy === "load";
  const locked = Boolean(busy) || Boolean(progress);
  const percent = progress && progress.total > 0
    ? Math.min(100, Math.round((progress.current / progress.total) * 100))
    : 0;

  return (
    <div className="app-shell">
      {notice && (
        <div className={`notice ${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>
          {notice.kind === "error" ? <X size={16} /> : <Check size={16} />}
          <span>{notice.text}</span>
        </div>
      )}

      <header className="topbar">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true">
            <ShieldCheck size={21} />
          </span>
          <h1>CSwitch</h1>
        </div>
        <button className="add-button" type="button" aria-label="添加供应商" title="添加供应商" disabled={locked} onClick={openAdd}>
          <Plus size={19} />
        </button>
      </header>

      <main>
        <section aria-label="官方登录">
          <article className={`provider-card official-card${state.officialActive ? " active" : ""}`}>
            <button className="provider-select" type="button" disabled={locked || state.officialActive} onClick={useOfficial}>
              <span className="provider-icon official-icon" aria-hidden="true">
                <CircleUserRound size={20} />
              </span>
              <span className="provider-copy">
                <span className="provider-title-row">
                  <strong>官方登录</strong>
                  {state.officialActive && (
                    <span className="active-badge"><Check size={12} /> 当前</span>
                  )}
                </span>
                <span className="provider-meta"><span>ChatGPT OAuth</span></span>
              </span>
              {busy === "official" ? <LoaderCircle className="spinner" size={18} /> : <LogIn size={18} />}
            </button>
            {busy === "official" && (
              <div className="provider-actions">
                <button className="cancel-login-button" type="button" onClick={cancelOfficial}>取消</button>
              </div>
            )}
          </article>

          <article className="auth-mode-card">
            <div className="auth-mode-copy">
              <strong>切换第三方时保留官方登录</strong>
              <p>
                {state.keepOfficialAuth
                  ? "当前会保留 ChatGPT 登录态，并把请求转发到所选 API Key。关闭后改为 API Key 直连。"
                  : "当前是 API Key 直连，会写入 auth.json。开启后保留 ChatGPT 登录，请求改走 API Key。"}
                {!state.officialAuthAvailable && " 建议先完成一次官方登录再开启。"}
              </p>
            </div>
            <button
              className={`switch${state.keepOfficialAuth ? " on" : ""}`}
              type="button"
              role="switch"
              aria-checked={state.keepOfficialAuth}
              aria-label="切换第三方时保留官方登录"
              disabled={locked}
              onClick={() => void toggleKeepOfficialAuth()}
            >
              <span />
            </button>
          </article>
        </section>

        <div className="section-heading">
          <h2>API 供应商</h2>
          <span className="count-badge">{state.providers.length}</span>
        </div>

        <section className="provider-list" aria-label="API 供应商列表" aria-busy={isLoading}>
          {isLoading && <div className="loading-row"><LoaderCircle className="spinner" size={22} /></div>}
          {!isLoading && state.providers.length === 0 && (
            <div className="empty-state">
              <Plus size={21} />
              <button type="button" onClick={openAdd}>添加第一个供应商</button>
            </div>
          )}
          {state.providers.map((provider) => (
            <ProviderCard
              key={provider.id}
              provider={provider}
              disabled={locked}
              activating={busy === provider.id}
              onActivate={() => void activate(provider)}
              onEdit={() => openEdit(provider)}
              onDelete={() => setDeletingProvider(provider)}
              onEnableRouting={() => setRoutingProvider(provider)}
            />
          ))}
        </section>
      </main>

      {providerDialogOpen && (
        <ProviderDialog
          provider={editingProvider}
          busy={busy === "save"}
          onCancel={() => {
            if (busy !== "save") setProviderDialogOpen(false);
          }}
          onSubmit={(draft) => void save(draft)}
        />
      )}

      {routingProvider && (
        <ConfirmDialog
          title="启用本地路由"
          message={`${routingProvider.name} 使用 ${routingProvider.protocol === "openai_chat" ? "Chat Completions" : "Anthropic Messages"}，需要转换为 Responses 协议。`}
          confirmLabel="启用并切换"
          busy={busy === "route"}
          onCancel={() => {
            if (busy !== "route") setRoutingProvider(null);
          }}
          onConfirm={() => void enableRouting()}
        />
      )}

      {progress && (
        <div className="progress-overlay" role="dialog" aria-modal="true" aria-labelledby="progress-title">
          <div className="progress-card">
            <h2 id="progress-title">{progress.title}</h2>
            <p className="progress-stage">{progress.stage}</p>
            {progress.detail && <p className="progress-detail">{progress.detail}</p>}
            <div className="progress-track" aria-hidden="true">
              <span style={{ width: `${percent}%` }} />
            </div>
            <div className="progress-meta">
              <span aria-live="polite">{progress.current}/{progress.total}</span>
              <span>{percent}%</span>
            </div>
            {progress.operation === "official" && (
              <button className="cancel-login-button" type="button" onClick={cancelOfficial}>
                取消官方登录
              </button>
            )}
          </div>
        </div>
      )}

      {deletingProvider && (
        <ConfirmDialog
          title="删除供应商"
          message={`确定删除 ${deletingProvider.name} 吗？保存的 API Key 和模型目录会一并删除。`}
          confirmLabel="删除"
          destructive
          busy={busy === "delete"}
          onCancel={() => {
            if (busy !== "delete") setDeletingProvider(null);
          }}
          onConfirm={() => void removeProvider()}
        />
      )}
    </div>
  );
}

export default App;
