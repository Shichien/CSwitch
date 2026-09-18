import {
  Check,
  CircleUserRound,
  Info,
  LoaderCircle,
  LogIn,
  Plus,
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
  ProviderSyncReport,
  SavedProvider,
} from "./types";

const EMPTY_STATE: ProviderState = {
  warnings: [],
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
  const shellRef = useRef<HTMLDivElement>(null);
  const officialCancelled = useRef(false);
  const inFlight = useRef(false);
  const beginAction = () => {
    if (inFlight.current || busy || progress) return false;
    inFlight.current = true;
    return true;
  };

  const showNotice = useCallback((kind: "success" | "error", text: string) => {
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    setNotice({ kind, text });
    noticeTimer.current = window.setTimeout(() => setNotice(null), 3000);
  }, []);

  const reportSuccess = (name: string, report: ProviderSyncReport) => {
    const text = switchedNotice(name, Math.max(report.rolloutFilesUpdated, report.sqliteRowsUpdated));
    showNotice(report.warnings?.length ? "error" : "success", [text, ...(report.warnings ?? [])].join("\n"));
  };

  const refresh = useCallback(async () => {
    setState(await cswitchApi.listProviders());
  }, []);

  useEffect(() => {
    if (state.warnings.length) showNotice("error", state.warnings.join("\n"));
  }, [state.warnings, showNotice]);

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
    let disposed = false;
    let unlistenProgress: (() => void) | undefined;
    let unlistenChanged: (() => void) | undefined;
    let unlistenError: (() => void) | undefined;
    void cswitchApi.onProgress((next) => {
      if (next.operation === "keep-auth") return;
      if (next.operation === "official" && officialCancelled.current) {
        if (next.done && !inFlight.current) setBusy(null);
        return;
      }
      setProgress(next.done ? null : next);
      if (!inFlight.current) {
        if (next.done) setBusy(null);
        else if (next.operation) setBusy(next.operation);
      }
    }).then((fn) => {
      if (disposed) fn(); else unlistenProgress = fn;
    });
    void cswitchApi.onProvidersChanged(() => {
      if (inFlight.current) return;
      void refresh().catch((error) => showNotice("error", errorText(error)));
    }).then((fn) => {
      if (disposed) fn(); else unlistenChanged = fn;
    });
    void cswitchApi.onOperationError((message) => {
      showNotice("error", message);
    }).then((fn) => {
      if (disposed) fn(); else unlistenError = fn;
    });
    return () => {
      disposed = true;
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
    if (!beginAction()) return;
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
      reportSuccess(provider.name, report);
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setProgress(null);
      setBusy(null);
    }
  };

  const save = async (draft: ProviderDraft) => {
    if (!beginAction()) return;
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
        const report = await cswitchApi.activateProvider(result.provider.id);
        await refresh();
        reportSuccess(result.provider.name, report);
        return;
      }
      showNotice("success", wasActive ? "供应商已更新并重新应用" : "供应商已保存");
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setProgress(null);
      setBusy(null);
    }
  };

  const enableRouting = async () => {
    if (!routingProvider || !beginAction()) return;
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
      reportSuccess(provider.name, report);
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setProgress(null);
      setBusy(null);
    }
  };

  const refreshModels = async (provider: ProviderSummary) => {
    if (!beginAction()) return;
    setBusy("models");
    try {
      setState(await cswitchApi.refreshProviderModels(provider.id));
      showNotice("success", `${provider.name} 的模型列表已刷新`);
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setBusy(null);
    }
  };

  const removeProvider = async () => {
    if (!deletingProvider || !beginAction()) return;
    setBusy("delete");
    try {
      await cswitchApi.deleteProvider(deletingProvider.id);
      setDeletingProvider(null);
      await refresh();
      showNotice("success", "供应商已删除");
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setBusy(null);
    }
  };

  const useOfficial = async () => {
    if (!beginAction()) return;
    officialCancelled.current = false;
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
      if (officialCancelled.current) showNotice("success", "切换已经完成，取消请求到达时操作已提交");
      else reportSuccess("官方登录", report);
    } catch (error) {
      const message = errorText(error);
      if (!officialCancelled.current && message !== "官方登录已取消") showNotice("error", message);
    } finally {
      inFlight.current = false;
      setProgress(null);
      setBusy(null);
    }
  };

  const toggleKeepOfficialAuth = async () => {
    if (!beginAction()) return;
    const enabled = !state.keepOfficialAuth;
    setBusy("keep-auth");
    try {
      setState(await cswitchApi.setKeepOfficialAuth(enabled));
      showNotice(
        "success",
        enabled
          ? "常驻路由已开启；首次启用请重新打开 Codex，后续切换线路直接生效"
          : "已关闭本地路由，请重新打开 Codex 使用原请求地址",
      );
    } catch (error) {
      showNotice("error", errorText(error));
    } finally {
      inFlight.current = false;
      setProgress(null);
      setBusy(null);
    }
  };

  const cancelOfficial = async () => {
    officialCancelled.current = true;
    setProgress(null);
    setBusy("official-cancelling");
    try {
      await cswitchApi.cancelOfficialLogin();
      showNotice("success", "已取消，正在结束当前操作");
    } catch (error) {
      showNotice("error", errorText(error));
    }
  };

  const isLoading = busy === "load";
  const locked = Boolean(busy) || Boolean(progress);
  const hasDialog = providerDialogOpen || Boolean(routingProvider) || Boolean(deletingProvider) || Boolean(progress);

  useEffect(() => {
    if (isLoading) return;
    const frame = window.requestAnimationFrame(() => {
      const shell = shellRef.current;
      if (!shell) return;
      const header = shell.querySelector<HTMLElement>(".topbar");
      const main = shell.querySelector<HTMLElement>("main");
      let height = (header?.offsetHeight ?? 0) + (main?.offsetHeight ?? 0);
      for (const panel of shell.querySelectorAll<HTMLElement>(".dialog-panel, .progress-card")) {
        height = Math.max(height, panel.scrollHeight + 42);
      }
      const limit = Math.max(220, Math.min(600, window.screen.availHeight - 80));
      height = Math.max(220, Math.min(limit, Math.ceil(height)));
      if (Math.abs(window.innerHeight - height) > 1) {
        void cswitchApi.resizeWindow(height).catch((error) => showNotice("error", errorText(error)));
      }
    });
    return () => window.cancelAnimationFrame(frame);
  }, [isLoading, state.providers.length, hasDialog, showNotice]);

  const percent = progress && progress.total > 0
    ? Math.min(100, Math.round((progress.current / progress.total) * 100))
    : 0;

  return (
    <div ref={shellRef} className={`app-shell${state.keepOfficialAuth ? " routing-active" : ""}`}>
      {notice && (
        <div className={`notice ${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>
          {notice.kind === "error" ? <X size={16} /> : <Check size={16} />}
          <span>{notice.text}</span>
        </div>
      )}

      <header className="topbar">
        <h1 className="sr-only">CSwitch</h1>
        <div className="route-control">
          <button
            id="resident-route"
            className={`switch${state.keepOfficialAuth ? " on" : ""}`}
            type="button"
            role="switch"
            aria-checked={state.keepOfficialAuth}
            aria-labelledby="resident-route-label"
            aria-busy={busy === "keep-auth"}
            disabled={locked}
            onClick={() => void toggleKeepOfficialAuth()}
          >
            <span />
          </button>
          <label id="resident-route-label" htmlFor="resident-route">常驻本地路由</label>
          <details
            className="route-help"
            onBlur={(event) => {
              if (!event.currentTarget.contains(event.relatedTarget)) event.currentTarget.open = false;
            }}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.currentTarget.open = false;
                event.currentTarget.querySelector("summary")?.focus();
              }
            }}
          >
            <summary aria-label="路由说明" title="路由说明"><Info size={16} /></summary>
            <div className="route-help-content">
              <strong>切换第三方时保留官方登录</strong>
              <p>
                {state.keepOfficialAuth
                  ? state.officialAuthAvailable ? "官方登录态已保留，请求使用当前供应商 API Key。" : "尚未保存官方登录，本次使用 API Key；完成官方登录后可保留。"
                  : "当前是 API Key 直连，会写入 auth.json。开启后保留 ChatGPT 登录，请求改走 API Key。"}
                {!state.officialAuthAvailable && " 建议先完成一次官方登录再开启。"}
              </p>
            </div>
          </details>
        </div>
        <button className="add-button" type="button" aria-label="添加供应商" title="添加供应商" disabled={locked} onClick={openAdd}>
          <Plus size={19} />
        </button>
      </header>

      <main>
        <section aria-label="官方登录">
          <article className={`provider-card official-card${state.officialActive ? " active" : ""}`}>
            <button className="provider-select" type="button" disabled={locked} onClick={useOfficial}>
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

        </section>

        <section className="provider-list" aria-label="API 供应商列表" aria-busy={isLoading}>
          <h2 className="sr-only">API 供应商</h2>
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
              onRefreshModels={() => void refreshModels(provider)}
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
