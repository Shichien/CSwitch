export type ProviderProtocol =
  | "openai_responses"
  | "openai_chat"
  | "anthropic_messages";

export interface ProviderSummary {
  id: string;
  name: string;
  apiUrl: string;
  modelCount: number;
  hasApiKey: boolean;
  active: boolean;
  protocol: ProviderProtocol | string;
  routingMode: "direct" | "local" | string;
}

export interface ProviderState {
  warnings: string[];
  providers: ProviderSummary[];
  activeProviderId: string | null;
  officialActive: boolean;
  keepOfficialAuth: boolean;
  officialAuthAvailable: boolean;
}

export interface OperationProgress {
  operation: string;
  title: string;
  stage: string;
  detail: string;
  current: number;
  total: number;
  done: boolean;
}

export interface SavedProvider {
  provider: ProviderSummary;
  routingRequired: boolean;
  routingMessage: string | null;
}

export interface ProviderSyncReport {
  rolloutFilesUpdated: number;
  sqliteRowsUpdated: number;
  backupPath: string;
  warnings: string[];
}

export interface ProviderDraft {
  name: string;
  apiUrl: string;
  apiKey: string;
}
