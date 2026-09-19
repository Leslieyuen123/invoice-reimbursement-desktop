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
  /** Local date the mail was received; the date batches are keyed on. */
  sourceReceivedDate: string | null;
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
  hasNormalizedPdf: boolean;
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
  /** Mails whose invoices still need attention (partial or failed extraction). */
  mailNeedsAttentionCount: number;
  recentBatches: BatchDto[];
}

export interface ItemFilter {
  status?: ItemStatus;
  sourceAccountId?: string;
  sourceUid?: number;
  recent?: boolean;
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
  /** Mark a mail as read once every invoice from it is in the library. */
  markProcessedMailSeen: boolean;
}

export interface StorageStatusDto {
  localDataDirectory: string;
  exportDirectory: string;
  availableBytes: number | null;
  recoveryError: string | null;
}

export interface ConsistencyIssueDto {
  key: string;
  label: string;
  count: number;
  hint: string;
  samples: string[];
}

export interface ConsistencyReportDto {
  checkedAt: string;
  itemsChecked: number;
  batchesChecked: number;
  issues: ConsistencyIssueDto[];
}

export interface ExportDiagnosticsInputDto {
  destination: string | null;
}

export interface DiagnosticsBundleDto {
  path: string;
  directory: string;
  bytes: number;
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

/** One batch member that currently blocks the export of the whole batch. */
export interface BatchIssueDto {
  itemId: string;
  fileName: string;
  code: string;
  message: string;
  repairable: boolean;
}

export interface BatchDetailDto {
  batch: BatchDto;
  items: InvoiceItemDto[];
  summary: BatchDetailSummaryDto;
  warnings: string[];
  issues: BatchIssueDto[];
}

/** How one scanned mail ended up in the invoice library. */
export type MailOutcome = "imported" | "partial" | "failed" | "ignored";

export interface MailLedgerEntryDto {
  accountId: string;
  mailbox: string;
  uid: number;
  subject: string | null;
  sender: string | null;
  receivedAt: string;
  processedAt: string;
  candidateCount: number;
  importedCount: number;
  existingCount: number;
  failedCount: number;
  outcome: MailOutcome;
  reason: string | null;
  markedSeen: boolean;
  /** Invoices are in the library but the mailbox still shows the mail unread. */
  seenMismatch: boolean;
}

export interface MailLedgerCountsDto {
  imported: number;
  partial: number;
  failed: number;
  ignored: number;
  needsAttention: number;
}

export interface MailLedgerCursorDto {
  receivedAt: string;
  uid: number;
  accountId: string;
}

export interface MailLedgerPageDto {
  items: MailLedgerEntryDto[];
  nextCursor: MailLedgerCursorDto | null;
}

export interface MailLedgerFilter {
  outcome?: MailOutcome;
  needsAttention?: boolean;
  accountId?: string;
  query?: string;
}

export interface MailLedgerPageRequestDto {
  cursor?: MailLedgerCursorDto;
  pageSize?: number;
}

export interface BatchRepairDto {
  repairedCount: number;
  issues: BatchIssueDto[];
}

/** One invoice the bulk confirm could not settle, with a stable reason code. */
export interface SkippedBatchItemDto {
  itemId: string;
  fileName: string;
  code: string;
  message: string;
}

export interface SettleBatchInputDto {
  fillInvoiceDateFromReceived: boolean;
  applySuggestedCategory: boolean;
  defaultCategory: Category | null;
}

export interface SettleBatchOutcomeDto {
  confirmedCount: number;
  filledInvoiceDateCount: number;
  appliedCategoryCount: number;
  repairedCount: number;
  skipped: SkippedBatchItemDto[];
  issues: BatchIssueDto[];
}

export type BatchCandidateDisabledReason =
  | "recognition_failed"
  | "suspected_duplicate"
  | "not_recognized"
  | "not_confirmed"
  | "missing_normalized_pdf"
  | "missing_original_file"
  | "incomplete_details";

export interface BatchCandidateDto {
  item: InvoiceItemDto;
  outsideBatchRange: boolean;
  eligible: boolean;
  disabledReason: BatchCandidateDisabledReason | null;
}

export interface UpdateBatchRangeInputDto {
  startDate: string;
  endDate: string;
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

export interface AccountAutomationFailureDto {
  accountId: string;
  email: string;
  message: string;
}

export interface BatchAutomationResultDto {
  scannedAccountCount: number;
  failedAccounts: AccountAutomationFailureDto[];
  importedCount: number;
  assignedCount: number;
  exceptionCount: number;
  repairedCount: number;
  export: ExportResultDto | null;
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
