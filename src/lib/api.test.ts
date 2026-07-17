import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { invokeMock } from "../test/mockApi";
import type {
  NewBatchInputDto,
  PreferencesInputDto,
  ReviewItemInputDto,
  SaveMailboxAccountInputDto,
  TestMailboxAccountInputDto,
} from "../types";
import { api } from "./api";

type ApiMethod = (...arguments_: unknown[]) => Promise<unknown>;

const reviewInput: ReviewItemInputDto = {
  id: "item-1",
  invoiceDate: "2026-07-15",
  suggestedPeriod: "2026-07",
  finalCategory: "dining",
  amountCents: 12_850,
  city: "上海",
  company: "示例餐厅",
  note: null,
  eventTag: null,
  projectTag: null,
};

const customBatchInput: NewBatchInputDto = {
  name: "差旅报销",
  startDate: "2026-07-01",
  endDate: "2026-07-31",
  note: null,
};

const saveAccountInput: SaveMailboxAccountInputDto = {
  id: null,
  provider: "gmail",
  email: "finance@example.com",
  secret: "app-password",
  imapHost: null,
  imapPort: null,
  enabled: true,
  syncIntervalMinutes: 15,
};

const testAccountInput: TestMailboxAccountInputDto = {
  id: null,
  provider: "gmail",
  email: "finance@example.com",
  secret: "app-password",
  imapHost: null,
  imapPort: null,
};

const preferencesInput: PreferencesInputDto = {
  backgroundSyncEnabled: true,
  exportDirectory: "/Users/finance/Exports",
  batchDirectoryPattern: "{batchName}-{timestamp}",
};

const commandCases: Array<{
  method: string;
  arguments_: unknown[];
  command: string;
  invokeArguments?: Record<string, unknown>;
}> = [
  { method: "getDashboard", arguments_: [], command: "get_dashboard" },
  {
    method: "listItems",
    arguments_: [
      { status: "pending_confirmation" },
      { cursor: { sortValue: "2026-07-16", id: "item-1" }, pageSize: 25 },
    ],
    command: "list_items",
    invokeArguments: {
      filter: { status: "pending_confirmation" },
      page: {
        cursor: { sortValue: "2026-07-16", id: "item-1" },
        pageSize: 25,
      },
    },
  },
  {
    method: "getItem",
    arguments_: ["item-1"],
    command: "get_item",
    invokeArguments: { itemId: "item-1" },
  },
  {
    method: "importManualFiles",
    arguments_: [["one.pdf", "two.pdf"]],
    command: "import_manual_files",
    invokeArguments: { paths: ["one.pdf", "two.pdf"] },
  },
  {
    method: "reviewItem",
    arguments_: [reviewInput],
    command: "review_item",
    invokeArguments: { input: reviewInput },
  },
  {
    method: "resolveDuplicate",
    arguments_: ["item-1", true],
    command: "resolve_duplicate",
    invokeArguments: { itemId: "item-1", keep: true },
  },
  {
    method: "retryRecognition",
    arguments_: ["item-1"],
    command: "retry_recognition",
    invokeArguments: { itemId: "item-1" },
  },
  {
    method: "listBatches",
    arguments_: [{ pageSize: 25 }],
    command: "list_batches",
    invokeArguments: { page: { pageSize: 25 } },
  },
  {
    method: "getBatch",
    arguments_: ["batch-1"],
    command: "get_batch",
    invokeArguments: { batchId: "batch-1" },
  },
  {
    method: "listBatchCandidates",
    arguments_: [
      "batch-1",
      "酒店",
      { cursor: { sortValue: "2026-07-16", id: "item-1" }, pageSize: 25 },
    ],
    command: "list_batch_candidates",
    invokeArguments: {
      batchId: "batch-1",
      query: "酒店",
      page: {
        cursor: { sortValue: "2026-07-16", id: "item-1" },
        pageSize: 25,
      },
    },
  },
  {
    method: "createMonthBatch",
    arguments_: [2026, 7],
    command: "create_month_batch",
    invokeArguments: { year: 2026, month: 7 },
  },
  {
    method: "createCustomBatch",
    arguments_: [customBatchInput],
    command: "create_custom_batch",
    invokeArguments: { input: customBatchInput },
  },
  {
    method: "assignItemsToBatch",
    arguments_: ["batch-1", ["item-1", "item-2"]],
    command: "assign_items_to_batch",
    invokeArguments: {
      batchId: "batch-1",
      itemIds: ["item-1", "item-2"],
    },
  },
  {
    method: "removeItemFromBatch",
    arguments_: ["batch-1", "item-1"],
    command: "remove_item_from_batch",
    invokeArguments: { batchId: "batch-1", itemId: "item-1" },
  },
  {
    method: "exportBatch",
    arguments_: ["batch-1"],
    command: "export_batch",
    invokeArguments: { batchId: "batch-1" },
  },
  {
    method: "listMailboxAccounts",
    arguments_: [],
    command: "list_mailbox_accounts",
  },
  {
    method: "saveMailboxAccount",
    arguments_: [saveAccountInput],
    command: "save_mailbox_account",
    invokeArguments: { input: saveAccountInput },
  },
  {
    method: "testMailboxAccount",
    arguments_: [testAccountInput],
    command: "test_mailbox_account",
    invokeArguments: { input: testAccountInput },
  },
  {
    method: "deleteMailboxAccount",
    arguments_: ["account-1"],
    command: "delete_mailbox_account",
    invokeArguments: { accountId: "account-1" },
  },
  { method: "getPreferences", arguments_: [], command: "get_preferences" },
  { method: "getStorageStatus", arguments_: [], command: "get_storage_status" },
  {
    method: "savePreferences",
    arguments_: [preferencesInput],
    command: "save_preferences",
    invokeArguments: { input: preferencesInput },
  },
  {
    method: "syncAccountNow",
    arguments_: ["account-1"],
    command: "sync_account_now",
    invokeArguments: { accountId: "account-1" },
  },
];

describe("desktop api", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
  });

  it("maps every Task 13 command to its exact wire name and arguments", async () => {
    const methods = api as unknown as Record<string, ApiMethod>;

    for (const commandCase of commandCases) {
      const method = methods[commandCase.method];
      expect(method, commandCase.method).toBeTypeOf("function");
      await method(...commandCase.arguments_);
      if (commandCase.invokeArguments) {
        expect(invokeMock).toHaveBeenLastCalledWith(
          commandCase.command,
          commandCase.invokeArguments,
        );
      } else {
        expect(invokeMock).toHaveBeenLastCalledWith(commandCase.command);
      }
    }
  });
});
