import type { ItemFilter, PageRequestDto } from "../types";

const itemLists = ["invoice-reimbursement", "items", "list"] as const;

export const queryKeys = {
  all: ["invoice-reimbursement"] as const,
  dashboard: ["invoice-reimbursement", "dashboard"] as const,
  itemLists,
  items: (filter: ItemFilter, page?: PageRequestDto) =>
    [...itemLists, filter, page] as const,
  item: (itemId: string) =>
    ["invoice-reimbursement", "items", "detail", itemId] as const,
  batches: (page?: PageRequestDto) =>
    ["invoice-reimbursement", "batches", page] as const,
  batch: (batchId: string) =>
    ["invoice-reimbursement", "batches", batchId] as const,
  mailboxAccounts: ["invoice-reimbursement", "mailbox-accounts"] as const,
  preferences: ["invoice-reimbursement", "preferences"] as const,
};
