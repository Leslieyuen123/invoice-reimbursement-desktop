import { invoke } from "@tauri-apps/api/core";

import type {
  BatchCandidateDto,
  BatchDetailDto,
  BatchDto,
  DashboardDto,
  ExportResultDto,
  InvoiceItemDto,
  ItemFilter,
  MailboxAccountDto,
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

export const api = {
  getDashboard: () => call<DashboardDto>("get_dashboard"),
  listItems: (filter: ItemFilter, page?: PageRequestDto) =>
    call<PageDto<InvoiceItemDto>>("list_items", { filter, page }),
  getItem: (itemId: string) =>
    call<InvoiceItemDto>("get_item", { itemId }),
  importManualFiles: (paths: string[]) =>
    call<ManualImportOutcomeDto[]>("import_manual_files", { paths }),
  reviewItem: (input: ReviewItemInputDto) =>
    call<InvoiceItemDto>("review_item", { input }),
  resolveDuplicate: (itemId: string, keep: boolean) =>
    call<InvoiceItemDto | null>("resolve_duplicate", { itemId, keep }),
  retryRecognition: (itemId: string) =>
    call<InvoiceItemDto>("retry_recognition", { itemId }),
  listBatches: (page?: PageRequestDto) =>
    call<PageDto<BatchDto>>("list_batches", { page }),
  getBatch: (batchId: string) =>
    call<BatchDetailDto>("get_batch", { batchId }),
  listBatchCandidates: (
    batchId: string,
    query?: string,
    page?: PageRequestDto,
  ) =>
    call<PageDto<BatchCandidateDto>>("list_batch_candidates", {
      batchId,
      query,
      page,
    }),
  createMonthBatch: (year: number, month: number) =>
    call<BatchDto>("create_month_batch", { year, month }),
  createCustomBatch: (input: NewBatchInputDto) =>
    call<BatchDto>("create_custom_batch", { input }),
  assignItemsToBatch: (batchId: string, itemIds: string[]) =>
    call<BatchDetailDto>("assign_items_to_batch", { batchId, itemIds }),
  removeItemFromBatch: (batchId: string, itemId: string) =>
    call<BatchDetailDto>("remove_item_from_batch", { batchId, itemId }),
  exportBatch: (batchId: string) =>
    call<ExportResultDto>("export_batch", { batchId }),
  listMailboxAccounts: () =>
    call<MailboxAccountDto[]>("list_mailbox_accounts"),
  saveMailboxAccount: (input: SaveMailboxAccountInputDto) =>
    call<MailboxAccountDto>("save_mailbox_account", { input }),
  testMailboxAccount: (input: TestMailboxAccountInputDto) =>
    call<void>("test_mailbox_account", { input }),
  deleteMailboxAccount: (accountId: string) =>
    call<void>("delete_mailbox_account", { accountId }),
  getPreferences: () => call<PreferencesDto>("get_preferences"),
  savePreferences: (input: PreferencesInputDto) =>
    call<PreferencesDto>("save_preferences", { input }),
  getStorageStatus: () => call<StorageStatusDto>("get_storage_status"),
  retryExportRecovery: () =>
    call<StorageStatusDto>("retry_export_recovery"),
  syncAccountNow: (accountId: string) =>
    call<void>("sync_account_now", { accountId }),
};
