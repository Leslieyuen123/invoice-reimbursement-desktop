import { invoke } from "@tauri-apps/api/core";

import type {
  ConsistencyReportDto,
  DiagnosticsBundleDto,
  ExportDiagnosticsInputDto,
  BatchCandidateDto,
  BatchAutomationResultDto,
  BatchDetailDto,
  BatchDto,
  BatchRepairDto,
  DashboardDto,
  SettleBatchInputDto,
  SettleBatchOutcomeDto,
  UpdateBatchRangeInputDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemFilter,
  MailboxAccountDto,
  MailLedgerCountsDto,
  MailLedgerFilter,
  MailLedgerPageDto,
  MailLedgerPageRequestDto,
  ManualImportOutcomeDto,
  NewBatchInputDto,
  PageDto,
  PageRequestDto,
  PreferencesDto,
  PreferencesInputDto,
  ReviewItemInputDto,
  SaveMailboxAccountInputDto,
  StorageStatusDto,
  TestMailboxAccountInputDto,
} from "../types";

type CommandArguments = Record<string, unknown>;
type CommandBridge = <T>(
  command: string,
  arguments_?: CommandArguments,
) => Promise<T>;

declare global {
  interface Window {
    __INVOICE_COMMAND_BRIDGE__?: CommandBridge;
  }
}

function call<T>(command: string, arguments_?: CommandArguments): Promise<T> {
  const developmentBridge =
    import.meta.env.DEV && typeof window !== "undefined"
      ? window.__INVOICE_COMMAND_BRIDGE__
      : undefined;
  if (developmentBridge) {
    return developmentBridge<T>(command, arguments_);
  }
  return arguments_ === undefined
    ? invoke<T>(command)
    : invoke<T>(command, arguments_);
}

export const API_COMMANDS = {
  getDashboard: "get_dashboard",
  listItems: "list_items",
  getItem: "get_item",
  openItemOriginal: "open_item_original",
  importManualFiles: "import_manual_files",
  reviewItem: "review_item",
  resolveDuplicate: "resolve_duplicate",
  retryRecognition: "retry_recognition",
  listBatches: "list_batches",
  getBatch: "get_batch",
  repairBatchNormalizedPdfs: "repair_batch_normalized_pdfs",
  updateBatchRange: "update_batch_range",
  settleBatchItems: "settle_batch_items",
  removeBatchItems: "remove_batch_items",
  listBatchCandidates: "list_batch_candidates",
  createMonthBatch: "create_month_batch",
  createCustomBatch: "create_custom_batch",
  assignItemsToBatch: "assign_items_to_batch",
  removeItemFromBatch: "remove_item_from_batch",
  exportBatch: "export_batch",
  runBatchAutomation: "run_batch_automation",
  listMailboxAccounts: "list_mailbox_accounts",
  saveMailboxAccount: "save_mailbox_account",
  testMailboxAccount: "test_mailbox_account",
  deleteMailboxAccount: "delete_mailbox_account",
  getPreferences: "get_preferences",
  savePreferences: "save_preferences",
  getStorageStatus: "get_storage_status",
  exportDiagnostics: "export_diagnostics",
  getConsistencyReport: "get_consistency_report",
  retryExportRecovery: "retry_export_recovery",
  syncAccountNow: "sync_account_now",
  listMailLedger: "list_mail_ledger",
  getMailLedgerCounts: "get_mail_ledger_counts",
} as const;

export function apiCommandNames() {
  return Object.values(API_COMMANDS).sort();
}

export const api = {
  getDashboard: () => call<DashboardDto>(API_COMMANDS.getDashboard),
  listItems: (filter: ItemFilter, page?: PageRequestDto) =>
    call<PageDto<InvoiceItemDto>>(API_COMMANDS.listItems, { filter, page }),
  getItem: (itemId: string) =>
    call<InvoiceItemDto>(API_COMMANDS.getItem, { itemId }),
  openItemOriginal: (itemId: string) =>
    call<void>(API_COMMANDS.openItemOriginal, { itemId }),
  importManualFiles: (paths: string[]) =>
    call<ManualImportOutcomeDto[]>(API_COMMANDS.importManualFiles, { paths }),
  reviewItem: (input: ReviewItemInputDto) =>
    call<InvoiceItemDto>(API_COMMANDS.reviewItem, { input }),
  resolveDuplicate: (itemId: string, keep: boolean) =>
    call<InvoiceItemDto | null>(API_COMMANDS.resolveDuplicate, { itemId, keep }),
  retryRecognition: (itemId: string) =>
    call<InvoiceItemDto>(API_COMMANDS.retryRecognition, { itemId }),
  listBatches: (page?: PageRequestDto) =>
    call<PageDto<BatchDto>>(API_COMMANDS.listBatches, { page }),
  getBatch: (batchId: string) =>
    call<BatchDetailDto>(API_COMMANDS.getBatch, { batchId }),
  listBatchCandidates: (
    batchId: string,
    query?: string,
    page?: PageRequestDto,
  ) =>
    call<PageDto<BatchCandidateDto>>(API_COMMANDS.listBatchCandidates, {
      batchId,
      query,
      page,
    }),
  createMonthBatch: (year: number, month: number) =>
    call<BatchDto>(API_COMMANDS.createMonthBatch, { year, month }),
  createCustomBatch: (input: NewBatchInputDto) =>
    call<BatchDto>(API_COMMANDS.createCustomBatch, { input }),
  assignItemsToBatch: (batchId: string, itemIds: string[]) =>
    call<BatchDetailDto>(API_COMMANDS.assignItemsToBatch, { batchId, itemIds }),
  removeItemFromBatch: (batchId: string, itemId: string) =>
    call<BatchDetailDto>(API_COMMANDS.removeItemFromBatch, { batchId, itemId }),
  repairBatchNormalizedPdfs: (batchId: string) =>
    call<BatchRepairDto>(API_COMMANDS.repairBatchNormalizedPdfs, { batchId }),
  updateBatchRange: (batchId: string, input: UpdateBatchRangeInputDto) =>
    call<BatchDetailDto>(API_COMMANDS.updateBatchRange, { batchId, input }),
  settleBatchItems: (batchId: string, input: SettleBatchInputDto) =>
    call<SettleBatchOutcomeDto>(API_COMMANDS.settleBatchItems, { batchId, input }),
  removeBatchItems: (batchId: string, itemIds: string[]) =>
    call<BatchDetailDto>(API_COMMANDS.removeBatchItems, { batchId, itemIds }),
  exportBatch: (batchId: string) =>
    call<ExportResultDto>(API_COMMANDS.exportBatch, { batchId }),
  runBatchAutomation: (batchId: string) =>
    call<BatchAutomationResultDto>(API_COMMANDS.runBatchAutomation, { batchId }),
  listMailboxAccounts: () =>
    call<MailboxAccountDto[]>(API_COMMANDS.listMailboxAccounts),
  saveMailboxAccount: (input: SaveMailboxAccountInputDto) =>
    call<MailboxAccountDto>(API_COMMANDS.saveMailboxAccount, { input }),
  testMailboxAccount: (input: TestMailboxAccountInputDto) =>
    call<void>(API_COMMANDS.testMailboxAccount, { input }),
  deleteMailboxAccount: (accountId: string) =>
    call<void>(API_COMMANDS.deleteMailboxAccount, { accountId }),
  getPreferences: () => call<PreferencesDto>(API_COMMANDS.getPreferences),
  savePreferences: (input: PreferencesInputDto) =>
    call<PreferencesDto>(API_COMMANDS.savePreferences, { input }),
  getStorageStatus: () => call<StorageStatusDto>(API_COMMANDS.getStorageStatus),
  exportDiagnostics: (input?: ExportDiagnosticsInputDto) =>
    call<DiagnosticsBundleDto>(API_COMMANDS.exportDiagnostics, { input }),
  getConsistencyReport: () =>
    call<ConsistencyReportDto>(API_COMMANDS.getConsistencyReport),
  retryExportRecovery: () =>
    call<StorageStatusDto>(API_COMMANDS.retryExportRecovery),
  syncAccountNow: (accountId: string) =>
    call<void>(API_COMMANDS.syncAccountNow, { accountId }),
  listMailLedger: (filter: MailLedgerFilter, page?: MailLedgerPageRequestDto) =>
    call<MailLedgerPageDto>(API_COMMANDS.listMailLedger, { filter, page }),
  getMailLedgerCounts: () =>
    call<MailLedgerCountsDto>(API_COMMANDS.getMailLedgerCounts),
};
