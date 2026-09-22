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

export interface OfficialAccountSummary {
  id: string;
  label: string;
  workspace: string;
  active: boolean;
  loginRetained: boolean;
}

export type OfficialAccountHealth = "valid" | "reauth_required" | "unknown";

export interface OfficialQuotaWindow {
  name: string;
  usedPercent: number;
  resetAt: number | null;
  windowSeconds: number | null;
}

export interface OfficialCredits {
  hasCredits: boolean;
  unlimited: boolean;
  balance: string | null;
}

export interface OfficialAccountUsage {
  accountId: string;
  health: OfficialAccountHealth;
  plan: string | null;
  windows: OfficialQuotaWindow[];
  credits: OfficialCredits | null;
  message: string | null;
  queriedAt: number;
}

export type ProviderUsageStatus =
  | "available"
  | "unsupported"
  | "unauthorized"
  | "unknown";

export interface ProviderUsage {
  providerId: string;
  status: ProviderUsageStatus;
  system: string | null;
  balance: number | null;
  total: number | null;
  used: number | null;
  unit: string | null;
  unlimited: boolean;
  plan: string | null;
  message: string | null;
  queriedAt: number;
}

export interface ProviderState {
  officialAccounts: OfficialAccountSummary[];
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
