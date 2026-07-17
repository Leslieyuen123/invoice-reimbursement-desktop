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

export type MailboxProvider = "gmail" | "qq";

export type MailboxErrorKind = "authentication" | "network" | "unknown";

export type AmountCents = number;

export const MAX_SAFE_AMOUNT_CENTS = Number.MAX_SAFE_INTEGER;

export interface CursorDto {
  sortValue: string;
  id: string;
}

export interface PageRequestDto {
  cursor?: CursorDto;
  pageSize?: number;
}

export interface PageDto<T> {
  items: T[];
  nextCursor: CursorDto | null;
}

export type AppError =
  | { code: "validation"; field: string; message: string }
  | { code: "not_found"; entity: string; message: string }
  | { code: "conflict"; message: string }
  | {
      code: "external";
      service: string;
      retryable: boolean;
      message: string;
    }
  | { code: "internal"; message: string };

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
  amountCents: AmountCents | null;
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
  totalAmountCents: AmountCents;
  unconfirmedCount: number;
  note: string | null;
  createdAt: string;
  updatedAt: string;
  lastExportedAt: string | null;
}

export interface MailboxAccountDto {
  id: string;
  provider: MailboxProvider;
  email: string;
  imapHost: string;
  imapPort: number;
  enabled: boolean;
  syncIntervalMinutes: number;
  lastSyncedAt: string | null;
  lastError: string | null;
  lastErrorKind: MailboxErrorKind | null;
  lastErrorAt: string | null;
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
  batchDirectoryPattern: string;
}

export interface StorageStatusDto {
  localDataDirectory: string;
  exportDirectory: string;
  availableBytes: number;
}

export interface ReviewItemInputDto {
  id: string;
  invoiceDate: string | null;
  suggestedPeriod: string;
  finalCategory: Category;
  amountCents: AmountCents;
  city: string | null;
  company: string | null;
  note: string | null;
  eventTag: string | null;
  projectTag: string | null;
}

export type ManualImportOutcomeDto =
  | { status: "imported"; path: string; item: InvoiceItemDto }
  | { status: "failed"; path: string; error: AppError };

export interface CategorySummaryDto {
  itemCount: number;
  amountCents: AmountCents;
}

export interface BatchDetailSummaryDto {
  itemCount: number;
  totalAmountCents: AmountCents;
  transport: CategorySummaryDto;
  dining: CategorySummaryDto;
  accommodation: CategorySummaryDto;
  hospitality: CategorySummaryDto;
  unconfirmedCount: number;
}

export interface BatchDetailDto {
  batch: BatchDto;
  items: InvoiceItemDto[];
  summary: BatchDetailSummaryDto;
  warnings: string[];
}

export type BatchCandidateDisabledReason =
  | "recognition_failed"
  | "suspected_duplicate";

export interface BatchCandidateDto {
  item: InvoiceItemDto;
  outsideBatchRange: boolean;
  eligible: boolean;
  disabledReason: BatchCandidateDisabledReason | null;
}

export interface NewBatchInputDto {
  name: string;
  startDate: string;
  endDate: string;
  note: string | null;
}

export interface ExportResultDto {
  directory: string;
  itemCount: number;
  totalAmountCents: AmountCents;
}

export interface SaveMailboxAccountInputDto {
  id: string | null;
  provider: MailboxProvider;
  email: string;
  secret: string;
  imapHost: string | null;
  imapPort: number | null;
  enabled: boolean;
  syncIntervalMinutes: number;
}

export interface TestMailboxAccountInputDto {
  id: string | null;
  provider: MailboxProvider;
  email: string;
  secret: string;
  imapHost: string | null;
  imapPort: number | null;
}

export type PreferencesInputDto = PreferencesDto;

export type BatchSummaryDto = BatchDto;
