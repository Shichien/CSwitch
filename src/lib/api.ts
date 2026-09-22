import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import type {
  OfficialAccountUsage,
  ProviderUsage,
  OperationProgress,
  ProviderDraft,
  ProviderState,
  ProviderSyncReport,
  SavedProvider,
} from "../types";

export const cswitchApi = {
  resizeWindow: (height: number) => getCurrentWindow().setSize(new LogicalSize(window.innerWidth, height)),
  listProviders: () => invoke<ProviderState>("list_providers"),
  queryOfficialAccountUsage: (accountId: string) =>
    invoke<OfficialAccountUsage>("query_official_account_usage", { accountId }),
  queryProviderUsage: (providerId: string) =>
    invoke<ProviderUsage>("query_provider_usage", { providerId }),
  saveProvider: (providerId: string | null, draft: ProviderDraft) =>
    invoke<SavedProvider>("save_provider", {
      providerId,
      name: draft.name,
      apiUrl: draft.apiUrl,
      apiKey: draft.apiKey,
    }),
  refreshProviderModels: (providerId: string) => invoke<ProviderState>("refresh_provider_models", { providerId }),
  activateProvider: (providerId: string) =>
    invoke<ProviderSyncReport>("activate_provider", { providerId }),
  enableProviderRouting: (providerId: string) =>
    invoke<void>("enable_provider_routing", { providerId }),
  deleteProvider: (providerId: string) =>
    invoke<void>("delete_provider", { providerId }),
  setKeepOfficialAuth: (enabled: boolean) =>
    invoke<ProviderState>("set_keep_official_auth", { enabled }),
  addOfficialAccount: () => invoke<ProviderSyncReport>("add_official_account"),
  activateOfficialAccount: (accountId: string) => invoke<ProviderSyncReport>("activate_official_account", { accountId }),
  startOfficialLogin: () =>
    invoke<ProviderSyncReport>("start_official_login"),
  cancelOfficialLogin: () => invoke<void>("cancel_official_login"),
  onProgress: (handler: (progress: OperationProgress) => void) =>
    listen<OperationProgress>("cswitch://operation-progress", (event) => handler(event.payload)),
  onProvidersChanged: (handler: () => void) =>
    listen("cswitch://providers-changed", () => handler()),
  onOperationError: (handler: (message: string) => void) =>
    listen<string>("cswitch://operation-error", (event) => handler(event.payload)),
};
