export type ItemStatus =
  | "pending_recognition"
  | "pending_confirmation"
  | "recognition_failed"
  | "suspected_duplicate"
  | "ready";

export type Category =
  | "transport"
  | "dining"
  | "accommodation"
  | "hospitality";

export type SourceType = "email" | "manual_upload";

export type BatchStatus = "draft" | "exported";

export type RecognitionStatus = "pending" | "succeeded" | "failed";

export type ConfirmationStatus = "pending" | "confirmed";

export type DedupeStatus = "unique" | "suspected_duplicate" | "resolved";

export interface InvoiceItemDto {
  id: string;
  originalName: string;
  previewUrl: string;
  sourceType: SourceType;
  sourceAccountId: string | null;
  fetchedAt: string;
  invoiceDate: string | null;
  suggestedPeriod: string | null;
  batchId: string | null;
  suggestedCategory: Category | null;
  finalCategory: Category | null;
  amountCents: number | null;
  currency: "CNY";
  city: string | null;
  company: string | null;
  status: ItemStatus;
  recognitionStatus: RecognitionStatus;
  confirmationStatus: ConfirmationStatus;
  dedupeStatus: DedupeStatus;
  note: string | null;
  eventTag: string | null;
  projectTag: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface BatchDto {
  id: string;
  name: string;
  startDate: string;
  endDate: string;
  status: BatchStatus;
  itemCount: number;
  totalAmountCents: number;
  unconfirmedCount: number;
  note: string | null;
  createdAt: string;
  updatedAt: string;
  lastExportedAt: string | null;
}

export interface MailboxAccountDto {
  id: string;
  provider: "gmail" | "qq";
  email: string;
  imapHost: string;
  imapPort: number;
  enabled: boolean;
  syncIntervalMinutes: number;
  lastSyncedAt: string | null;
  lastError: string | null;
}

export interface DashboardDto {
  mailboxAccounts: MailboxAccountDto[];
  recentlyAddedCount: number;
  pendingConfirmationCount: number;
  recognitionFailedCount: number;
  suspectedDuplicateCount: number;
  recentBatches: BatchDto[];
}

export interface ItemFilter {
  status?: ItemStatus;
  suggestedPeriod?: string;
  category?: Category;
  sourceType?: SourceType;
  batchId?: string;
  query?: string;
}

export interface PreferencesDto {
  backgroundSyncEnabled: boolean;
  exportDirectory: string;
  batchDirectoryPattern: "{batchName}-{timestamp}";
}
