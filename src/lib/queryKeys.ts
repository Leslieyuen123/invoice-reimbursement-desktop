import type {
  ItemFilter,
  MailLedgerFilter,
  MailLedgerPageRequestDto,
  PageRequestDto,
} from "../types";

const itemLists = ["invoice-reimbursement", "items", "list"] as const;
const batchLists = ["invoice-reimbursement", "batches", "list"] as const;
const mailLedgerLists = ["invoice-reimbursement", "mail-ledger", "list"] as const;

export const queryKeys = {
  all: ["invoice-reimbursement"] as const,
  dashboard: ["invoice-reimbursement", "dashboard"] as const,
  consistencyReport: ["invoice-reimbursement", "consistency"] as const,
  itemLists,
  items: (filter: ItemFilter, page?: PageRequestDto) =>
    [...itemLists, filter, page] as const,
  item: (itemId: string) =>
    ["invoice-reimbursement", "items", "detail", itemId] as const,
  batchLists,
  batches: (page?: PageRequestDto) =>
    [...batchLists, page] as const,
  batch: (batchId: string) =>
    ["invoice-reimbursement", "batches", "detail", batchId] as const,
  batchContentRevision: (batchId: string) =>
    ["invoice-reimbursement", "batches", "content-revision", batchId] as const,
  batchCandidateLists: (batchId: string) =>
    [
      "invoice-reimbursement",
      "batches",
      "detail",
      batchId,
      "candidates",
    ] as const,
  batchCandidates: (
    batchId: string,
    query: string,
    page?: PageRequestDto,
  ) =>
    [
      "invoice-reimbursement",
      "batches",
      "detail",
      batchId,
      "candidates",
      query,
      page,
    ] as const,
  mailboxAccounts: ["invoice-reimbursement", "mailbox-accounts"] as const,
  preferences: ["invoice-reimbursement", "preferences"] as const,
  storageStatus: ["invoice-reimbursement", "storage-status"] as const,
  mailLedger: (filter: MailLedgerFilter, page?: MailLedgerPageRequestDto) =>
    [...mailLedgerLists, filter, page] as const,
  mailLedgerCounts: ["invoice-reimbursement", "mail-ledger", "counts"] as const,
};
