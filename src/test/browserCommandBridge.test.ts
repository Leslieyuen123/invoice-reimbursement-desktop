import { describe, expect, it } from "vitest";

import type {
  BatchDetailDto,
  BatchDto,
  ExportResultDto,
  InvoiceItemDto,
  ManualImportOutcomeDto,
  PageDto,
  StorageStatusDto,
} from "../types";
import {
  browserCommandNames,
  createBrowserCommandBridge,
} from "./browserCommandBridge";

const frontendCommands = [
  "assign_items_to_batch",
  "create_custom_batch",
  "create_month_batch",
  "delete_mailbox_account",
  "export_batch",
  "get_batch",
  "get_dashboard",
  "get_item",
  "get_preferences",
  "get_storage_status",
  "import_manual_files",
  "list_batch_candidates",
  "list_batches",
  "list_items",
  "list_mailbox_accounts",
  "remove_item_from_batch",
  "resolve_duplicate",
  "retry_export_recovery",
  "retry_recognition",
  "review_item",
  "save_mailbox_account",
  "save_preferences",
  "sync_account_now",
  "test_mailbox_account",
] as const;

describe("browser command bridge", () => {
  it("has an exact handler for every frontend wire command", () => {
    expect(browserCommandNames()).toEqual(frontendCommands);
  });

  it("keeps the manual review, batch assignment, and export flow stateful", async () => {
    const bridge = createBrowserCommandBridge();

    const emptyItems = await bridge<PageDto<InvoiceItemDto>>("list_items", {
      filter: {},
    });
    expect(emptyItems).toEqual({ items: [], nextCursor: null });

    const imports = await bridge<ManualImportOutcomeDto[]>(
      "import_manual_files",
      { paths: ["text-invoice.pdf"] },
    );
    expect(imports).toHaveLength(1);
    expect(imports[0]).toMatchObject({
      status: "imported",
      path: "text-invoice.pdf",
      item: {
        originalName: "text-invoice.pdf",
        previewUrl: "/src-tauri/tests/fixtures/image-invoice.png",
        status: "pending_confirmation",
      },
    });
    if (imports[0].status !== "imported") throw new Error("fixture import failed");

    const reviewed = await bridge<InvoiceItemDto>("review_item", {
      input: {
        id: imports[0].item.id,
        invoiceDate: "2026-07-15",
        suggestedPeriod: "2026-07",
        finalCategory: "dining",
        amountCents: 12_850,
        city: "上海",
        company: "测试餐厅",
        note: null,
        eventTag: null,
        projectTag: null,
      },
    });
    expect(reviewed).toMatchObject({
      finalCategory: "dining",
      status: "ready",
      confirmationStatus: "confirmed",
    });

    const batch = await bridge<BatchDto>("create_month_batch", {
      year: 2026,
      month: 7,
    });
    const candidates = await bridge<
      PageDto<{ item: InvoiceItemDto; outsideBatchRange: boolean; eligible: boolean }>
    >("list_batch_candidates", { batchId: batch.id });
    expect(candidates.items).toEqual([
      expect.objectContaining({
        item: expect.objectContaining({ id: reviewed.id }),
        outsideBatchRange: false,
        eligible: true,
      }),
    ]);

    const assigned = await bridge<BatchDetailDto>("assign_items_to_batch", {
      batchId: batch.id,
      itemIds: [reviewed.id],
    });
    expect(assigned.summary).toMatchObject({
      itemCount: 1,
      totalAmountCents: 12_850,
      unconfirmedCount: 0,
      dining: { itemCount: 1, amountCents: 12_850 },
    });

    const exported = await bridge<ExportResultDto>("export_batch", {
      batchId: batch.id,
    });
    expect(exported).toEqual({
      directory: "/tmp/invoice-reimbursement/e2e/2026-07",
      itemCount: 1,
      totalAmountCents: 12_850,
    });
    await expect(
      bridge<BatchDetailDto>("get_batch", { batchId: batch.id }),
    ).resolves.toMatchObject({ batch: { status: "exported" } });
  });

  it("implements every frontend command and rejects unknown commands clearly", async () => {
    const bridge = createBrowserCommandBridge();

    await expect(bridge("get_dashboard")).resolves.toMatchObject({
      pendingConfirmationCount: 0,
      recentBatches: [],
    });
    await expect(bridge("list_mailbox_accounts")).resolves.toEqual([]);
    await expect(bridge("get_preferences")).resolves.toMatchObject({
      backgroundSyncEnabled: true,
    });
    await expect(bridge<StorageStatusDto>("get_storage_status")).resolves.toMatchObject({
      recoveryError: null,
    });
    await expect(bridge("retry_export_recovery")).resolves.toMatchObject({
      recoveryError: null,
    });
    await expect(bridge("sync_account_now", { accountId: "missing" })).rejects.toMatchObject({
      code: "not_found",
    });
    await expect(bridge("not_a_real_command")).rejects.toThrow(
      "Unsupported browser command: not_a_real_command",
    );
  });

  it("injects deterministic delay and command failures for browser state audits", async () => {
    const bridge = createBrowserCommandBridge({
      delayMs: 20,
      failCommands: new Set(["list_items"]),
    });
    const startedAt = performance.now();

    await expect(
      bridge("list_items", { filter: {} }),
    ).rejects.toMatchObject({
      code: "external",
      service: "browser_bridge",
      retryable: true,
      message: "Simulated browser command failure: list_items",
    });
    expect(performance.now() - startedAt).toBeGreaterThanOrEqual(15);
    await expect(bridge("list_batches")).resolves.toEqual({
      items: [],
      nextCursor: null,
    });
  });
});
