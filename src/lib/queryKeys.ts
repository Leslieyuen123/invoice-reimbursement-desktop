import type { ItemFilter, PageRequestDto } from "../types";

export const queryKeys = {
  all: ["invoice-reimbursement"] as const,
  dashboard: ["invoice-reimbursement", "dashboard"] as const,
  items: (filter: ItemFilter, page?: PageRequestDto) =>
    ["invoice-reimbursement", "items", filter, page] as const,
  item: (itemId: string) =>
    ["invoice-reimbursement", "items", itemId] as const,
  batches: (page?: PageRequestDto) =>
    ["invoice-reimbursement", "batches", page] as const,
  batch: (batchId: string) =>
    ["invoice-reimbursement", "batches", batchId] as const,
  mailboxAccounts: ["invoice-reimbursement", "mailbox-accounts"] as const,
  preferences: ["invoice-reimbursement", "preferences"] as const,
};
